//! The repeated-verifiers ceiling counts distinct candidate generations: two
//! verdicts over one unchanged tree are evidence about the machinery, not about
//! the model (ADR-0178).
//!
//! The ceiling exists to say "the member keeps producing work that fails the
//! same way". Between two verdicts over one tree the member produced nothing at
//! all, so the second says nothing of the kind — and counting it hands a member
//! a terminal Work wedge for a repeat it never made.
//!
//! That happened. On 2026-08-26 a stale checkout meant `retention-archive-tier`'s
//! gate judged the same effective content twice; the identical failure pair
//! (`verify.clippy` + `verify.test`) tripped the ceiling and wedged the member
//! with `wedge_cause` Work, though no new model work had been judged since the
//! first failure. The sibling fix binds a Verify's checkout to the candidate the
//! order judges and makes that particular route unreachable; this is the
//! defence behind it, so a *different* producer of one repeated tree cannot
//! spend the member's ceiling either.
//!
//! Four repair laps, five failing verdicts of one verifier, over four distinct
//! trees — the first lap deliberately re-deposits the construct's capture. The
//! sealed Verify budget is three repair rolls, so the rule moves the wedge one
//! judged generation later: pre-fix the fourth verdict is the third counted
//! repeat and wedges the member there, which is where this run stops.

use core::iter::once;

use aether_bloomery::{BloomId, CandidateRef, VerifyFailure, VerifyFailureSet, WedgeCause};
use aether_harness_bloomery::{FixtureHarness, captured, digest, failed};

/// The workpiece the single sealed member covers.
const WORKPIECE: &str = "wp";

/// The verifier every verdict names. One identity throughout is what makes each
/// verdict past the first a *repeat* — the axis this scenario is about is which
/// of those repeats the ceiling is entitled to count.
fn clippy() -> VerifyFailureSet {
    once(VerifyFailure::Clippy).collect()
}

#[test]
fn a_verify_that_fails_twice_over_one_tree_spends_no_repair_roll() {
    let mut harness = FixtureHarness::start("one-tree-repeat");
    let bloom = harness.seal_member(WORKPIECE, digest(0x51));

    let construct = harness.await_order();
    let first = harness.seed_capture(bloom, WORKPIECE, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, first));

    // A novel identity costs no roll, so this failure is forgiven whichever
    // rule is in force. It is what puts the first tree on the record as the
    // generation the series is counting over.
    fail_the_verify(&mut harness, bloom);

    // The lap that produced nothing: the same capture, deposited again. Its
    // verdict repeats `verify.clippy` over content no repair touched, so it
    // buys no repair roll and is recorded as the machinery anomaly it is.
    repair_with(&mut harness, first);
    fail_the_verify(&mut harness, bloom);
    let member = &harness.bloom(bloom).members[0];
    assert!(member.wedge.is_none(), "a repeat over one tree is not the model failing twice");
    assert_eq!(member.machinery_rolls, 1, "it is the machinery serving one tree twice, and is counted there");

    // Two genuine generations follow. Each repeats the identity over content
    // the member did change, so each spends a roll — two of the sealed three.
    for tree in [0xC2, 0xC3] {
        let generation = harness.seed_capture(bloom, WORKPIECE, digest(tree), digest(tree + 0x10));
        repair_with(&mut harness, generation);
        fail_the_verify(&mut harness, bloom);
    }
    assert!(
        harness.bloom(bloom).members[0].wedge.is_none(),
        "three counted repeats wedge the member; the lap that changed nothing was not one of them",
    );

    // The third counted repeat is the ceiling, and it is a Work wedge naming
    // the identity that actually repeated across generations.
    let last = harness.seed_capture(bloom, WORKPIECE, digest(0xC4), digest(0xD4));
    repair_with(&mut harness, last);
    fail_the_verify(&mut harness, bloom);

    let member = harness.bloom(bloom).members[0].clone();
    assert_eq!(
        member.wedge.expect("the ceiling is still a ceiling").repeated_verifiers,
        clippy(),
        "a failure over a generation the member really produced still counts",
    );
    assert_eq!(member.wedge_cause, Some(WedgeCause::Work), "and it is the model's work the wedge is about");
}

/// Answer the outstanding member Verify with a failing verdict naming `clippy`.
fn fail_the_verify(harness: &mut FixtureHarness, bloom: BloomId) {
    let verify = harness.await_order();
    assert_eq!(
        verify.displayed_digest,
        harness.bloom(bloom).members[0]
            .cursor
            .as_ref()
            .and_then(|cursor| cursor.candidate)
            .expect("the member is verifying a capture")
            .tree
            .as_bytes()
            .to_vec(),
        "the verdict binds the candidate the cursor holds",
    );
    harness.upload_admitted(&failed(&verify, clippy()));
}

/// Answer the outstanding repair lap with `candidate` as its capture.
fn repair_with(harness: &mut FixtureHarness, candidate: CandidateRef) {
    let refine = harness.await_order();
    harness.upload_admitted(&captured(&refine, candidate));
}
