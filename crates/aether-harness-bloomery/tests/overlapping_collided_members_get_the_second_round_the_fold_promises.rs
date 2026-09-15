//! Two members that overlap on one file collide in the same fold and reconcile
//! against the one settled checkpoint blind of each other. The one whose
//! reconciled candidate still collides takes the second round the fold's own
//! design promises rather than wedging on it, and the bloom lands.
//!
//! #4952 settles the fold before anything reconciles, so both colliders are
//! handed the same tree and neither can see the other's repair — this scenario
//! asserts that first, off the two collisions the fold really reported. The
//! fold's own comment accepts what follows: "a member reconciled against an
//! intermediate tree pays a second round for a collision it never had". The
//! Reconcile budget of one never funded that round, so it arrived as an
//! ADR-0189 section 5 wedge. On 2026-09-14 two collided members both appended
//! to the same four files, their reconciled commits still collided in fifteen
//! keep-both hunks, and the operator built the union by hand (#5993).
//!
//! The repeat is admitted here the way the lane-write observation is, rather
//! than waited on: what the fixture's fold does with a second armed collision
//! depends on ordering this scenario does not control, and the claim to pin is
//! the door's — that a repeat naming a head a sibling is still standing on buys
//! a lap instead of a wedge, and that the dispatch, intake and landing on the
//! far side of it are real.

#![allow(clippy::unwrap_used)]

use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{
    BloomId, BloomStatus, Digest, Evidence, Fact, Outcome, StageId, WorkpieceId, decode_recorded_event,
};
use aether_chassis_bloomery::store::{OutstandingOrder, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed};

const FIRST: &str = "wp-0";
const SECOND: &str = "wp-1";
const THIRD: &str = "wp-2";
const MEMBERS: [&str; 3] = [FIRST, SECOND, THIRD];
const COLLIDERS: [&str; 2] = [SECOND, THIRD];
const SHARED: &str = "crates/example-shared/src/lib.rs";
const OBSERVED_AT_MILLIS: u64 = 1_700_000_000_000;

/// How long one step waits for the order it named.
const ORDER_BUDGET: Duration = Duration::from_secs(20);

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

fn named<'a>(orders: &'a [OutstandingOrder], workpiece: &str) -> &'a OutstandingOrder {
    orders
        .iter()
        .find(|order| order.workpiece == workpiece)
        .unwrap_or_else(|| panic!("no outstanding order for {workpiece}"))
}

/// Admit the observation the executor's working-tree sweep would have admitted
/// for one lane. Every member writes the one shared file, so every pair of them
/// overlaps.
fn observe(harness: &mut FixtureHarness, bloom: BloomId, workpiece: &str) {
    let fact = Fact::LaneWritesObserved {
        bloom,
        workpiece: WorkpieceId(workpiece.to_owned()),
        stage: StageId::Construct,
        paths: vec![SHARED.to_owned()],
        observed_at: OBSERVED_AT_MILLIS,
    };
    match harness.admit(&format!("writes-{workpiece}"), fact) {
        Outcome::LeasesObserved { .. } => {}
        other => panic!("the lane-write observation must be admitted: {other:?}"),
    }
}

/// Every collision the fold reported, as `(workpiece, checkpoint, head, evidence)`.
fn reported_collisions(harness: &FixtureHarness) -> Vec<(String, Digest, Digest, Evidence)> {
    harness
        .commission_store()
        .replay_journal()
        .expect("the coordinator journal replays")
        .iter()
        .filter_map(|row| decode_recorded_event(&row.event, row.event_schema.as_deref()).ok())
        .filter_map(|event| match event.fact {
            Fact::FoldConflict { workpiece, checkpoint, head, evidence, .. } => {
                Some((workpiece.0, checkpoint, head, evidence))
            }
            _ => None,
        })
        .collect()
}

/// Dispatch until one exact `(workpiece, stage)` order is outstanding, and
/// return it. Both colliders' laps can stand at once, so a scenario that counts
/// outstanding orders would be reading the other member's.
fn await_named(harness: &mut FixtureHarness, workpiece: &str, stage: StageId) -> OutstandingOrder {
    let deadline = Instant::now() + ORDER_BUDGET;
    loop {
        harness.dispatch_tick();
        if let Some(order) =
            harness.orders().into_iter().find(|order| order.workpiece == workpiece && stage_of(order) == stage)
        {
            return order;
        }
        assert!(Instant::now() < deadline, "no {stage:?} order for {workpiece} inside {ORDER_BUDGET:?}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Answer one member's authoring lap with a tree of its own, then pass the
/// Verify it advances to.
fn author_and_verify(harness: &mut FixtureHarness, bloom: BloomId, order: &OutstandingOrder, seed: u8) {
    let candidate = harness.seed_capture(bloom, &order.workpiece, digest(seed), digest(seed.wrapping_add(1)));
    harness.upload_admitted(&captured(order, candidate));

    let verify = await_named(harness, &order.workpiece, StageId::Verify);
    harness.upload_admitted(&passed(&verify));
}

#[test]
fn overlapping_collided_members_get_the_second_round_the_fold_promises() {
    let mut harness = FixtureHarness::start("overlapping-collided-members");
    let bloom = harness.seal_members(&[(FIRST, digest(0x51)), (SECOND, digest(0x52)), (THIRD, digest(0x53))]);

    let constructs = harness.await_orders(3);
    for (index, workpiece) in MEMBERS.iter().enumerate() {
        observe(&mut harness, bloom, workpiece);
        let candidate = harness.seed_capture(
            bloom,
            workpiece,
            digest(0xC0 + u8::try_from(index).expect("three members") * 2),
            digest(0xC1 + u8::try_from(index).expect("three members") * 2),
        );
        harness.upload_admitted(&captured(named(&constructs, workpiece), candidate));
    }
    let verifies = harness.await_orders(3);
    for workpiece in MEMBERS {
        harness.upload_admitted(&passed(named(&verifies, workpiece)));
    }

    // The hunks really do overlap, so both later members collide with the fold
    // the first member settled.
    for workpiece in COLLIDERS {
        harness.seed_fold_conflict(bloom, workpiece, vec![SHARED.to_owned()]);
    }
    harness.integrate_tick();
    harness.dispatch_tick();

    let collisions = reported_collisions(&harness);
    assert_eq!(
        collisions.iter().map(|(workpiece, ..)| workpiece.as_str()).collect::<Vec<_>>(),
        COLLIDERS,
        "both overlapping members collide in the one fold: {collisions:?}",
    );
    let (_, checkpoint, head, evidence) = collisions[0].clone();
    assert!(
        collisions.iter().all(|(_, point, at, _)| *point == checkpoint && *at == head),
        "#4952 settles the fold first, so both are handed the same checkpoint: {collisions:?}",
    );

    // `hold_overlapping_reconcile` serializes the second collider's lap behind
    // the first one's resolution, so only one is dispatched — and it buys
    // nothing, because the checkpoint the held lap will be handed is the one
    // this fold settled either way. Disarm the seeded collisions here: what the
    // fixture's fold would make of a second armed one depends on ordering this
    // scenario does not control, and the repeat below is admitted rather than
    // waited on for exactly that reason.
    let lap = await_named(&mut harness, SECOND, StageId::Reconcile);
    for workpiece in COLLIDERS {
        harness.clear_fold_conflict(bloom, workpiece);
    }
    author_and_verify(&mut harness, bloom, &lap, 0xD0);

    // The repeat: the reconciled candidate still collides, and the tree it is
    // handed is the one it already reconciled onto because nothing folded in
    // between — its sibling's lap is still out, so the checkpoint has not
    // moved. That sibling still standing on the same head is what makes this
    // the round the fold promised rather than the member failing to reproduce
    // its own intent.
    let repeat = harness.admit(
        "recollide-second",
        Fact::FoldConflict {
            bloom,
            workpiece: WorkpieceId(SECOND.to_owned()),
            checkpoint,
            head,
            evidence: Evidence { detail: digest(0xAB), ..evidence },
        },
    );
    assert!(
        matches!(repeat, Outcome::FoldConflictDispatched { .. }),
        "the promised second round is funded rather than wedged: {repeat:?}",
    );

    // The held collider's own lap released behind its sibling's first
    // resolution, and the granted round now queues behind it in turn — the hold
    // orders the laps, it does not decide whether one is bought.
    let sibling_lap = await_named(&mut harness, THIRD, StageId::Reconcile);
    author_and_verify(&mut harness, bloom, &sibling_lap, 0xF0);

    let granted = await_named(&mut harness, SECOND, StageId::Reconcile);
    assert_ne!(granted.nonce, lap.nonce, "the granted round is a fresh lap, not the one already answered");
    author_and_verify(&mut harness, bloom, &granted, 0xE0);

    harness.land_the_fold(bloom);
    let view = harness.bloom(bloom);
    assert_eq!(view.status, BloomStatus::Landed, "the bloom lands on the far side of the second round");
    for member in &view.members {
        assert!(member.wedge.is_none(), "no member wedged on a collision the fold's own design promised: {member:?}");
    }
}
