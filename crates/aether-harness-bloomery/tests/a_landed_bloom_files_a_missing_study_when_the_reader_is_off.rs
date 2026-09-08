//! A bloom lands on a host that does not run the reader, and the study it never
//! got is journaled as missing rather than silently absent.
//!
//! The other arm of the sibling scenario, and the one the knob exists for.
//! ADR-0216 §4 leaves the reader's seat to the owner, but the ADR-0214
//! provenance gate validates every instruction field before any model lane
//! dispatches — so the first authorized bundle has to carry the reader's text,
//! and a bundle that carries it would otherwise start a standing opus read on
//! every landing the moment construct and review became dispatchable. The knob
//! is what keeps those two decisions apart.
//!
//! What is not visible from inside the reducer is that the decline happens at
//! the drain and still reaches the journal. The landing decides the `Study`
//! dispatch either way — nothing upstream of the executor knows about the knob
//! — so an off host that simply skipped the row would leave a decided dispatch
//! parked forever and a bloom whose study is missing with no record saying so.
//! Both halves are asserted here: no order is spent, and the journal carries the
//! read's own failing verdict.

#![allow(clippy::unwrap_used)]

use aether_bloomery::BloomStatus;
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed};

const MEMBER: &str = "wp-0";

#[test]
fn a_landed_bloom_files_a_missing_study_when_the_reader_is_off() {
    // The production default, stated rather than inherited: this scenario is
    // about the off arm, so it must not pass by accident if the default moves.
    let mut harness = FixtureHarness::start("study-reader-off");
    let bloom = harness.seal_member(MEMBER, digest(0x51));

    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, MEMBER, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, candidate));

    let verify = harness.await_order();
    harness.upload_admitted(&passed(&verify));

    harness.land_the_fold(bloom);

    harness.pump_until("the declined read reaches the journal", |harness| !harness.study_verdicts(bloom).is_empty());

    assert_eq!(
        harness.study_verdicts(bloom),
        [false],
        "the declined read is journaled once, as a study that did not pass",
    );
    let outstanding = harness.orders();
    assert!(
        outstanding.is_empty(),
        "no order is spent on a read this host declined: {:?}",
        outstanding.iter().map(|order| order.nonce.clone()).collect::<Vec<_>>(),
    );

    let view = harness.bloom(bloom);
    assert_eq!(view.status, BloomStatus::Landed, "a declined read leaves the bloom landed");
    assert!(view.members.iter().all(|member| member.wedge.is_none()), "and wedges no member: {:?}", view.members);
    assert!(
        view.executor_fault.is_none(),
        "the decline is the reader's own; folding it into the aggregate-review series would render a landed bloom as \
         host-stalled: {:?}",
        view.executor_fault,
    );
}
