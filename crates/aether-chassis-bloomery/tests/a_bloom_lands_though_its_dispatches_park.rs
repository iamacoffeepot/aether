//! Seal to landed with every dispatch parked in the adapter — the loop still
//! closes when the executor's blocking calls are genuinely slow (#5564).
//!
//! The executor no longer makes its adapter calls on the dispatch tick; it
//! hands each one to a worker, reports it in flight, and consumes the answer on
//! a later turn. Every phase of the loop therefore gained a third outcome it
//! must handle without losing anything: an outbox entry whose submit has not
//! answered stays unacked, a tracked handle whose inspect has not answered
//! stays unobserved, an expired order whose cancel has not answered stays live.
//! Each of those is a place a dropped want, an over-eager ack, or a completion
//! wake that never arrives would wedge the bloom — silently, because nothing
//! errors: the coordinator simply stops advancing.
//!
//! A run where every call answers instantly never separates "consumed on this
//! turn" from "consumed on the next one", so it cannot see any of that. Parking
//! the dispatch does: each stage's submit is answered by a worker while the
//! turn that asked has already moved on, and the member advances only if the
//! re-ask, the ack prefix, and the completion wake all hold.
//!
//! The mirror of `a_bloom_with_all_scripted_verdicts_lands`, which runs the
//! same chain at speed; what this one adds is the delay, so keep it minimal
//! otherwise.

use std::time::Duration;

use aether_bloomery::BloomStatus;
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed};

/// The workpiece the single sealed member covers.
const WORKPIECE: &str = "wp";

/// How long each armed dispatch parks inside the fixture's `workflow_dispatch`.
///
/// Long enough that the answer cannot land on the turn that asked for it —
/// which is the whole separation this scenario exists to exercise — and short
/// enough that four of them stay well inside the fixture cell's step budget.
const STALL: Duration = Duration::from_millis(750);

#[test]
fn a_bloom_lands_though_each_dispatch_parks_in_the_adapter() {
    let mut harness = FixtureHarness::start("parked-dispatch-scenario");
    let scope_revision = digest(0x51);
    let sealed_on = harness.view().mainline;

    // The seal's own base-verify dispatch is the first to park.
    harness.stall_next_dispatch(STALL);
    let bloom = harness.seal_member(WORKPIECE, scope_revision);

    // Construct: the seal's dispatch decision is in the outbox, and the turn
    // that drains it hands the submit to a worker rather than waiting on it.
    // The order row is written as `submitting` before the call runs, so session
    // reuse can still resolve it, but `await_order` (and every other "waiting
    // on a run" reader) only sees it once the worker answers and the row is
    // promoted. What has to hold is that the entry is not acked until the
    // answer lands and the handle is tracked.
    harness.stall_next_dispatch(STALL);
    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, WORKPIECE, digest(0xC1), digest(0xC2));
    harness.upload_admitted(&captured(&construct, candidate));

    // Verify: the passing Construct moved the cursor and dispatched the next
    // stage, which parks the same way.
    harness.stall_next_dispatch(STALL);
    let verify = harness.await_order();
    harness.upload_admitted(&passed(&verify));

    // The claim set is complete, so the git-side fold, the bloom-level gates,
    // and the landing follow — the first of them through one more parked
    // dispatch.
    harness.stall_next_dispatch(STALL);
    harness.land_the_fold(bloom);

    assert_ne!(harness.view().mainline, sealed_on, "mainline advanced off the base the bloom sealed on");
    assert_eq!(
        harness.bloom(bloom).status,
        BloomStatus::Landed,
        "a bloom whose every dispatch answered from a worker still lands",
    );
}
