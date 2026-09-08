//! A bloom lands, the coordinator dispatches the reader over what it landed,
//! and a reader that never reaches a verdict leaves the bloom landed.
//!
//! Two claims, and neither is visible from inside the reducer. The first is
//! that the dispatch survives the seam: `Study` is decided in the same decision
//! set that releases every membership the bloom held, and the executor drain's
//! ordinary liveness check reads exactly that table — so a reader gated the way
//! its aggregate siblings are gated would be retired undispatched on every tick
//! and no unit test of the reducer would notice. The second is that the read is
//! not a gate: ADR-0216 says a failed or faulted reader resolves the bloom with
//! its study missing, and the only place that is decidable is here, where a
//! real order is really answered and the bloom's own projection is read back.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{BloomStatus, LandingReceipt, StageId, Transformation};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, faulted, passed};

const MEMBER: &str = "wp-0";

#[test]
fn a_landed_bloom_is_read_and_a_faulted_read_does_not_hold_it() {
    let mut harness = FixtureHarness::start("study-after-land");
    let base = harness.view().mainline;
    let bloom = harness.seal_member(MEMBER, digest(0x51));

    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, MEMBER, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, candidate));

    let verify = harness.await_order();
    harness.upload_admitted(&passed(&verify));

    harness.land_the_fold(bloom);
    let landed = harness.view().mainline;

    // Exactly one order, and it is the reader: `await_orders` refuses a second,
    // so a landing that decided two reads — or re-decided one on a later tick —
    // fails here rather than spending a second opus lane per bloom.
    let study = harness.await_order();
    assert_eq!(from_bytes::<StageId>(&study.stage).unwrap(), StageId::Study);
    assert!(study.workpiece.is_empty(), "the reader is a bloom-level order with no member axis");

    let order = from_bytes::<Transformation>(&study.transformation).unwrap();
    assert_eq!(order.command, "retrospect.read");
    assert_eq!(
        order.inputs,
        [LandingReceipt { bloom, previous_base: base, new_head: landed }.digest()],
        "the order pins the receipt the land produced, which is what its evidence binds to",
    );
    assert_eq!(order.checkout, landed, "the reader checks out the head mainline moved to");
    assert_eq!(order.diff_base, Some(base), "and reads it against the base the bloom sealed on");

    // The read faults: the executor reached no verdict at all, which is the
    // worst case ADR-0216 names — and the one a reader modelled on the
    // aggregate gates would answer with a retry or a wedge.
    harness.upload_admitted(&faulted(&study));

    assert!(harness.orders().is_empty(), "a faulted read consumes its order rather than leaving one outstanding");
    let view = harness.bloom(bloom);
    assert_eq!(view.status, BloomStatus::Landed, "a faulted read leaves the bloom landed");
    assert!(view.members.iter().all(|member| member.wedge.is_none()), "and wedges no member: {:?}", view.members);
    assert!(view.review_park.is_none(), "and parks nothing");
    assert!(
        view.executor_fault.is_none(),
        "the reader's fault is its own; folding it into the aggregate-review series would render a landed bloom as \
         host-stalled: {:?}",
        view.executor_fault,
    );
    assert!(
        view.composition.as_ref().is_none_or(|composition| composition.wedge.is_none()),
        "and leaves the composition unwedged: {:?}",
        view.composition,
    );
}
