//! Two members verify green alone, their candidates refuse to build together on
//! the fold, and the member that happens to be verified on that fold is charged
//! with none of it: the composition narrows to the two candidates that collide
//! and repairs their coexistence itself (ADR-0210).
//!
//! Tonight, on bloom `abd504afd855`. One member added a test calling
//! `.contains()` in `xtask`; a sibling collapsed the value it calls into an
//! `EvidenceChannel` enum in the same tree. Each was green alone. The first
//! member verified on the fold of both was a console member whose surface is
//! `crates/aether-bloomery-console/**`, and every lever the reducer held was
//! member-shaped, so the verdict landed on it. Its Refine correctly declined to
//! repair code it never wrote and the member parked for three hours.
//!
//! Pre-fix, the failing verdict admitted as `Fact::VerifyFailed` against the
//! verified member: its cursor moved to `Refine`, it spent a repair roll, and no
//! composition was minted at all. The whole path from the classifier's answer to
//! the dispatched repair crosses the executor → intake → reducer → projection
//! seam, which is the only place it can be observed.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{StageId, WorkpieceId};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, Oracle, captured, digest, narrowed, passed};

/// The member that added the caller, in `xtask`.
const CALLER: &str = "wp-0";
/// The member that retyped the value under it, in the same tree.
const DEFINITION: &str = "wp-1";
/// The console member that touched neither and was verified on the fold.
const VERIFIED: &str = "wp-2";

const USE_SITE: &str = "xtask/src/transform/verify/mod.rs";
const DEFINITION_SITE: &str = "xtask/src/transform/mod.rs";

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

fn named<'a>(orders: &'a [OutstandingOrder], workpiece: &str) -> &'a OutstandingOrder {
    orders
        .iter()
        .find(|order| order.workpiece == workpiece)
        .unwrap_or_else(|| panic!("no outstanding order for {workpiece}"))
}

#[test]
fn a_fold_two_candidates_refuse_narrows_to_a_composition_over_them() {
    let mut harness = FixtureHarness::start("fold-narrows-to-its-parents");
    let bloom = harness.seal_members(&[(CALLER, digest(0x51)), (DEFINITION, digest(0x52)), (VERIFIED, digest(0x53))]);

    let constructs = harness.await_orders(3);
    for (workpiece, tree, checkout) in [(CALLER, 0xC1, 0xD1), (DEFINITION, 0xC2, 0xD2), (VERIFIED, 0xC3, 0xD3)] {
        let capture = harness.seed_capture(bloom, workpiece, digest(tree), digest(checkout));
        harness.upload_admitted(&captured(named(&constructs, workpiece), capture));
    }

    // Each of the two colliding candidates is green on its own — which is what
    // makes this a collision rather than a defect: nothing either member did is
    // wrong, and neither Verify saw the other's work.
    let verifies = harness.await_orders(3);
    harness.upload_admitted(&passed(named(&verifies, CALLER)));
    harness.upload_admitted(&passed(named(&verifies, DEFINITION)));

    // The third member is verified on the fold that now holds both, and the
    // tree refuses to build. The diagnostic names two files, both of them in
    // the other two members' work and neither of them in this one's.
    let verified_order = named(&verifies, VERIFIED);
    harness.upload_admitted(&narrowed(
        verified_order,
        &[CALLER, DEFINITION],
        &[USE_SITE, DEFINITION_SITE],
        &["crates/aether-bloomery/**", "xtask/**"],
    ));

    let view = harness.bloom(bloom);

    // The defect, named: the member that happened to verify the fold owes
    // nothing for it. Pre-fix its cursor moved to Refine and it spent a repair
    // roll on a lap it would decline.
    let innocent = view.members.iter().find(|member| member.workpiece.0 == VERIFIED).expect("the member is listed");
    assert_eq!(
        innocent.cursor.as_ref().map(|cursor| cursor.stage),
        Some(StageId::Verify),
        "the verified member stays where it was: {innocent:?}",
    );
    assert!(innocent.wedge.is_none(), "a collision it did not cause is not its wedge: {innocent:?}");
    assert!(innocent.awaiting_surface.is_none(), "and it is not asked to widen its surface either: {innocent:?}");

    // The collision has a subject of its own, and that subject names who caused
    // it and what it may touch.
    let narrowed = view.narrowed_compositions.first().expect("the fold narrowed to a composition over its parents");
    assert!(narrowed.workpiece.is_composition(), "the subject is a composition: {narrowed:?}");
    assert_eq!(
        narrowed.parents,
        vec![WorkpieceId(CALLER.to_owned()), WorkpieceId(DEFINITION.to_owned())],
        "the parents are the two candidates that collide, in canonical order",
    );
    assert!(
        !narrowed.parents.contains(&WorkpieceId(VERIFIED.to_owned())),
        "and never the member that happened to verify the fold",
    );
    assert_eq!(narrowed.paths, vec![DEFINITION_SITE.to_owned(), USE_SITE.to_owned()]);
    assert_eq!(
        narrowed.bound,
        vec!["crates/aether-bloomery/**".to_owned(), "xtask/**".to_owned()],
        "the bound is the union of exactly its parents' approved surfaces, and carries nothing of the verified member's",
    );

    // And it is working: the composition dispatches its own lane against the
    // refused tree, so the bloom moves without a person.
    let repair = harness.await_order();
    assert!(repair.workpiece.starts_with(WorkpieceId::COMPOSITION), "the repair is the composition's: {repair:?}");
    assert_eq!(stage_of(&repair), StageId::Refine, "one merge mechanism means one repair stage");

    // And it finishes: the repair produces a candidate that makes both intents
    // coexist. The composition does not dispatch its own Verify — no admitted
    // fact can complete a composition at that stage — and the member whose
    // verdict minted the narrowing is put back on its own Verify against the
    // repaired tree. Its original refusal judged a tree that has since been
    // redone, so leaving it holding one would strand it with nothing in flight.
    let repair_capture = harness.seed_capture(bloom, &repair.workpiece, digest(0xCF), digest(0xDF));
    harness.upload_admitted(&captured(&repair, repair_capture));
    let reverify = harness.await_order();
    assert_eq!(reverify.workpiece, VERIFIED, "the only dispatch is the verified member's: {reverify:?}");
    assert_eq!(stage_of(&reverify), StageId::Verify, "the innocent member is back on the line: {reverify:?}");

    harness.upload_admitted(&passed(&reverify));

    for member in &harness.bloom(bloom).members {
        assert!(member.wedge.is_none(), "no member wedged over a collision the composition owned: {member:?}");
        assert!(member.awaiting_surface.is_none(), "and none was asked to widen its surface: {member:?}");
    }

    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));
}
