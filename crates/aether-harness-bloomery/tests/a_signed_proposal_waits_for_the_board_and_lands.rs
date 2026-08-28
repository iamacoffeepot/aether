//! A proposal admitted mid-bloom that sealed immediately would put the
//! operator's commit underneath members whose subjects were cut before it,
//! which is the incident ADR-0205 was written from.

#![allow(clippy::unwrap_used)]

use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{BloomStatus, Outcome, StageId};
use aether_harness_bloomery::{FixtureHarness, OperatorMove, captured, digest, passed};

const MEMBER: &str = "wp-0";

#[test]
fn a_signed_proposal_waits_for_the_board_and_lands() {
    let mut harness = FixtureHarness::start("signed-proposal-waits");
    let bloom = harness.seal_members(&[(MEMBER, digest(0x51))]);

    let constructs = harness.await_orders(1);
    let candidate = harness.seed_capture(bloom, MEMBER, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&constructs[0], candidate));

    let proposal_candidate = harness.seed_capture(bloom, "operator-proposal", digest(0xE1), digest(0xE2));
    let outcome = harness.apply_operator(
        bloom,
        &OperatorMove::Propose {
            at_tick: 0,
            candidate: proposal_candidate,
            reason: "flip an ADR status".into(),
            operator: "harness".into(),
        },
    );
    assert!(
        matches!(&outcome, Outcome::ProposalQueued { offered: false, .. }),
        "a proposal admitted while a member walks waits: {outcome:?}"
    );
    assert_eq!(
        harness.view().blooms.iter().filter(|bloom| bloom.status == BloomStatus::Sealed).count(),
        1,
        "the proposal seals no bloom while the member is unresolved: {:?}",
        harness.view().blooms
    );

    let verify = harness.await_order();
    assert_eq!(verify.workpiece, MEMBER);
    harness.upload_admitted(&passed(&verify));
    harness.land_the_fold(bloom);
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed);
    let member_head = harness.view().mainline;

    let proposal_bloom = await_proposal_bloom(&mut harness);
    let view = harness.bloom(proposal_bloom);
    assert!(view.members.is_empty(), "a proposal bloom has no members: {view:?}");
    let composition = view.composition.as_ref().expect("the composition sits at Verify");
    let cursor = composition.cursor.as_ref().expect("the composition has a cursor");
    assert_eq!(cursor.stage, StageId::Verify);
    assert_eq!(cursor.candidate, Some(proposal_candidate));

    let orders = harness.await_orders(1);
    assert!(orders[0].workpiece.is_empty(), "the mechanical gate is bloom-level: {:?}", orders[0]);
    harness.upload_admitted(&passed(&orders[0]));
    harness.await_landing(proposal_bloom, BloomStatus::Landed);
    assert_ne!(
        harness.view().mainline,
        member_head,
        "landing the proposal advances mainline off the member bloom's head"
    );
}

fn await_proposal_bloom(harness: &mut FixtureHarness) -> aether_bloomery::BloomId {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        harness.propose_tick();
        if let Some(bloom) =
            harness.view().blooms.iter().find(|bloom| bloom.members.is_empty() && bloom.status == BloomStatus::Sealed)
        {
            return bloom.id;
        }
        assert!(Instant::now() < deadline, "the proposal bloom did not seal: {:?}", harness.view().blooms);
        thread::sleep(Duration::from_millis(20));
    }
}
