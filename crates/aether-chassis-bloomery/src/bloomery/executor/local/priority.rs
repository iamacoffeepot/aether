//! Which waiting dispatch takes the next free lane slot (#5410).
//!
//! [`affinity`](super::affinity) answers *where* a dispatch runs; this answers
//! *when*. The two compose: a member that reaches Refine while its construct's
//! slot is still free resumes in place on a warm target, and it only stays free
//! if the refine is handed a slot before the constructs queued ahead of it take
//! one.
//!
//! Submission order alone gets that wrong. A bloom seals one dispatch per member
//! at once, so the queue fills with constructs in the first seconds and every
//! stage a member reaches afterwards arrives behind all of them. A refine lane
//! is the member's own construct session resumed against the candidate it just
//! built, so while it waits its session ages towards the pool's cutoffs, the
//! slot that compiled its tree is handed to a stranger, and a member that is one
//! repair from landing is parked behind work that has not started.
//!
//! So the queue is ordered by what a dispatch continues rather than by when it
//! arrived: a stage resuming a live thread first, a stage judging a candidate
//! that already exists next, a stage starting something new last. Within a band
//! submission order is untouched.
//!
//! Starvation is bounded by the shape of a bloom rather than by a rule here. A
//! member reaches Refine only after a Construct that already ran, so the higher
//! bands are drawn from members already in flight and drain as those members
//! land; they cannot be replenished without the lowest band running first.

use aether_bloomery::StageId;

/// The order the queue hands out lane slots in. Lower sorts sooner, so the
/// declaration order below *is* the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DispatchPriority {
    /// Continues a thread that is already alive. Reconcile and Refine resume
    /// the member's own construct session against the tree that session built,
    /// so every second spent queueing is warmth decaying: the session ages
    /// towards the reuse cutoffs and the slot holding its target directory is
    /// handed to someone else.
    Resume,
    /// Judges a candidate that already exists. Nothing is warm to lose, but a
    /// member is held at a stage it cannot leave, and what it is waiting behind
    /// has not begun.
    Judge,
    /// Starts something new. The band that can always be started later, because
    /// nothing is waiting on it yet.
    Start,
}

/// The band `stage` dispatches in.
///
/// `None` — a dispatch whose stage this host cannot resolve, which is every
/// dispatch on a backend with no message store mounted — takes [`Start`]. That
/// is the band submission order already put it in: with the stage unknown for
/// every dispatch, all of them tie and the queue is the strict FIFO it was.
///
/// [`Start`]: DispatchPriority::Start
#[must_use]
pub fn priority_of(stage: Option<StageId>) -> DispatchPriority {
    match stage {
        Some(StageId::Refine | StageId::Reconcile) => DispatchPriority::Resume,
        Some(
            StageId::Verify
            | StageId::AggregateVerify
            | StageId::BaseVerify
            | StageId::Review
            | StageId::AggregateReview,
        ) => DispatchPriority::Judge,
        Some(
            StageId::Construct
            | StageId::Sketch
            | StageId::Scope
            | StageId::Approve
            | StageId::Integrate
            | StageId::Land
            | StageId::Study,
        )
        | None => DispatchPriority::Start,
    }
}

/// Which waiting dispatch takes the slot that just freed: the highest band, and
/// within it the one that has waited longest.
///
/// A linear scan over the queue, which is bounded by the members one bloom
/// seals; the alternative — a heap keyed by (band, arrival) — would buy nothing
/// at that size and would cost the queue its readability on the cancel path,
/// which removes by nonce.
#[must_use]
pub fn next_waiting(waiting: &[DispatchPriority]) -> Option<usize> {
    waiting.iter().enumerate().min_by_key(|(index, priority)| (**priority, *index)).map(|(index, _)| index)
}

/// Whether a dispatch of `priority` must join the queue rather than start on a
/// free slot right now.
///
/// It must whenever something already waiting would be handed the slot first —
/// its own band or better. A dispatch that outranks everything waiting takes the
/// slot inline, which is the whole point: a refine arriving at a host with a
/// free slot and a queue full of constructs is exactly the case the ordering
/// exists for, and making it queue behind them would give the ordering away at
/// the door.
#[must_use]
pub fn must_wait_behind(priority: DispatchPriority, waiting: &[DispatchPriority]) -> bool {
    waiting.iter().any(|queued| *queued <= priority)
}

#[cfg(test)]
mod tests {
    use super::{DispatchPriority, must_wait_behind, next_waiting, priority_of};
    use aether_bloomery::StageId;

    #[test]
    fn a_refine_takes_the_slot_ahead_of_a_verify_and_a_construct_that_queued_first() {
        // Tripwire for the ordering itself. On bloom f063ff066e83 three members
        // reached Refine while six slots were held by four constructs and two
        // verifies admitted earlier, and none of the three dispatched for
        // minutes: the queue was strict FIFO, so the stage with a live session
        // and a warm slot waited behind the stages with neither.
        let queue =
            [DispatchPriority::Start, DispatchPriority::Judge, DispatchPriority::Start, DispatchPriority::Resume];

        assert_eq!(next_waiting(&queue), Some(3), "the refine goes first however late it arrived");
        assert_eq!(next_waiting(&queue[..3]), Some(1), "then the verify");
        assert_eq!(next_waiting(&queue[..1]), Some(0));
        assert_eq!(next_waiting(&[]), None, "an empty queue hands out nothing");
    }

    #[test]
    fn submission_order_still_decides_inside_one_band() {
        // The ordering is between bands and nowhere else. A band that reordered
        // its own arrivals would make two constructs race on nothing, and the
        // queue would stop being explicable from the log line each submit wrote.
        let queue = [DispatchPriority::Start, DispatchPriority::Resume, DispatchPriority::Resume];

        assert_eq!(next_waiting(&queue), Some(1), "the earlier of two refines");
        assert_eq!(next_waiting(&[DispatchPriority::Start; 3]), Some(0), "an all-construct queue is untouched FIFO");
    }

    #[test]
    fn a_dispatch_waits_only_behind_what_would_beat_it_to_the_slot() {
        // Both halves of the door. A construct must not overtake anything, or
        // the ordering the pump applies is undone by every fresh submit; a
        // refine must overtake a queue of constructs, or it is applied only to
        // slots that free after the refine is already parked.
        let constructs = [DispatchPriority::Start, DispatchPriority::Start];

        assert!(must_wait_behind(DispatchPriority::Start, &constructs), "a peer already waiting has the prior claim");
        assert!(!must_wait_behind(DispatchPriority::Resume, &constructs), "a refine outranks a queue of constructs");
        assert!(
            must_wait_behind(DispatchPriority::Resume, &[DispatchPriority::Resume]),
            "and never overtakes another refine",
        );
        assert!(!must_wait_behind(DispatchPriority::Start, &[]), "nothing waiting is nothing to wait behind");
    }

    #[test]
    fn every_stage_lands_in_the_band_its_work_belongs_to() {
        // Tripwire: the mapping is the policy, and a stage silently sorted into
        // the wrong band is invisible until a bloom runs slowly for a reason
        // nothing states. An unresolvable stage must land in `Start`, which is
        // where submission order already had it.
        assert_eq!(priority_of(Some(StageId::Refine)), DispatchPriority::Resume);
        assert_eq!(priority_of(Some(StageId::Reconcile)), DispatchPriority::Resume);
        assert_eq!(priority_of(Some(StageId::Verify)), DispatchPriority::Judge);
        assert_eq!(priority_of(Some(StageId::Review)), DispatchPriority::Judge);
        assert_eq!(priority_of(Some(StageId::Construct)), DispatchPriority::Start);
        assert_eq!(priority_of(None), DispatchPriority::Start, "an unknown stage keeps the FIFO it had");
    }
}
