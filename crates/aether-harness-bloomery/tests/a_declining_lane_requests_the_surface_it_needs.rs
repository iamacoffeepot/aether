//! A construct-family lane that cannot finish inside its declared surface
//! names the paths it needs, the member parks awaiting a person, and the
//! operator's amendment puts it back on the line carrying the surface they
//! granted (ADR-0207).
//!
//! The sibling of `a_declining_refine_lane_leaves_a_visible_park`: that one
//! pins the plain, request-less park; this one pins the typed request and the
//! answer. The whole lane → executor → intake → reducer → projection → `/view`
//! path is real, which is the only place either can be observed at all — the
//! paths cross four crates between the evidence file and the served document.
//!
//! Pre-fix (the park): nothing read the lane's request. A decline reached
//! `/view` as a bare park with no remedy attached, so an operator saw a stopped
//! member and no statement of what would unstop it.
//!
//! Pre-fix (the amendment): the work order a lane reads is a rendered copy of
//! its scope revision, and an amendment widens that revision's
//! `declared_surface` while carrying every other field — the description the
//! copy is rendered from included — across unchanged. The renderer returned the
//! stored body verbatim, so the re-dispatched lap read the exact surface it had
//! just declined against, declined again, and this time asked for nothing: its
//! paths were inside its own revision by then, so the request normalized away
//! and the lap fell through to a park naming no remedy.

#![allow(clippy::unwrap_used)]

use aether_bloomery::testing::digest;
use aether_bloomery::{CONSTRUCT_IMPLEMENT_COMMAND, Outcome, StageId, WorkpieceId, digest_of};
use aether_chassis_bloomery::bloomery::mock_lane::REQUESTED_PATH;
use aether_harness_bloomery::{BloomeryHarness, HarnessBuilder, LaneScript, OperatorMove, Oracle, Repo};

#[test]
fn a_declining_lane_requests_the_surface_it_needs() {
    let mut harness = BloomeryHarness::start();
    harness.script_lane(&WorkpieceId("wp".into()), StageId::Construct, &[LaneScript::DeclineRequestingSurface]);
    let bloom = harness.seal_member("wp", digest(0x51));

    harness
        .run_until(|harness| harness.bloom(bloom).members.iter().any(|member| member.awaiting_surface.is_some()), 40);

    let view = harness.bloom(bloom);
    let member = &view.members[0];
    let awaiting = member.awaiting_surface.as_ref().expect("the request reaches the served document");

    // The paths and their reasons are on the document, so an operator reads
    // what to widen without opening an evidence file.
    assert_eq!(awaiting.paths.len(), 1, "the lane asked for exactly what it named: {awaiting:?}");
    assert_eq!(awaiting.paths[0].path, REQUESTED_PATH);
    assert!(!awaiting.paths[0].reason.is_empty(), "a requested path carries the line that justifies it");
    assert_eq!(awaiting.scope_revision, member.scope_revision, "a request is bound to the revision it amends");
    assert_eq!(awaiting.requests, 1);

    // The three park classes stay distinguishable where an operator reads
    // them: a decline *with* a request lights this field and nothing else.
    assert!(member.wedge.is_none(), "asking for surface is not a wedge");
    assert!(member.pending_decision.is_none(), "asking for surface is not an ADR-0151 question");

    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));

    // The park holds: the coordinator does not re-run a refusal a second lap
    // would reproduce verbatim, and no attempt burns while it waits. Nothing
    // widens the surface either — an increase is an operator's decision, so the
    // member waits for one however cheap the delta looks.
    let before = harness.bloom(bloom).members[0].cursor.clone();
    for _ in 0..5 {
        harness.tick();
    }
    let after = harness.bloom(bloom).members[0].clone();
    assert_eq!(after.cursor, before, "a parked member spends no attempt and does not move");
    assert!(after.awaiting_surface.is_some(), "the park persists until it is answered");
    assert!(harness.outstanding().is_empty(), "nothing was dispatched against the parked member");
}

#[test]
fn an_amended_surface_reaches_the_re_dispatched_lane() {
    let repo = Repo::with_example_project();
    let mut harness = HarnessBuilder::local_authority(&repo).hold_repo(repo).start("bloomery-harness");
    harness.script_lane(&WorkpieceId("wp".into()), StageId::Construct, &[LaneScript::DeclineRequestingSurface]);

    // A real scope revision, because the amendment reads the surface it widens
    // out of the commission store and writes the successor back into it — and
    // because the work order the lane reads is that revision rendered.
    let sealed = harness.author_scope_revision("wp", &["crates/example-a/**"]);
    let bloom = harness.seal_member("wp", sealed);
    harness.run_until(|harness| harness.bloom(bloom).members[0].awaiting_surface.is_some(), 40);

    let parked = harness.bloom(bloom);
    let awaiting = parked.members[0].awaiting_surface.as_ref().expect("the lane's request reaches the document");
    assert_eq!(awaiting.paths[0].path, REQUESTED_PATH, "the park names what to widen: {awaiting:?}");

    let widened = harness
        .scope_revision(sealed)
        .expect("the sealed revision is in the commission store")
        .with_widened_surface(&["crates/example-b/**".to_owned()]);

    // The operator answers, through the store writes and the supersession
    // `cargo xtask bloom amend` performs. Nothing the coordinator runs widens a
    // surface on its own, so this is the only way the park ends.
    let successor = harness.amend_surface(bloom, "wp", &["crates/example-b/**"]);

    // Ticked directly rather than through `run_until`, which consults the
    // liveness oracle on every still tick: the fixture lane's candidate writes
    // paths no scenario declares, so the line past this construct lap walks
    // into a containment failure and a Verify wedge. What this scenario is
    // about ends at the lap, and the park before it was already oracle-checked.
    let mut member = harness.bloom(successor).members[0].clone();
    for _ in 0..60 {
        if member.cursor.as_ref().is_some_and(|cursor| cursor.candidate.is_some()) {
            break;
        }
        harness.tick();
        member = harness.bloom(successor).members[0].clone();
    }

    assert!(member.awaiting_surface.is_none(), "the amendment answered the request: {member:?}");
    assert_eq!(member.scope_revision, digest_of(&widened), "the member is pinned at the widened revision");
    assert!(
        member.cursor.as_ref().is_some_and(|cursor| cursor.candidate.is_some()),
        "the re-armed member walked its construct stage to a candidate: {member:?}",
    );

    // Stored is not delivered. The work order is a rendered copy of the
    // revision, so an amendment that widens the field and re-renders the body
    // it carried forward hands the re-dispatched lane the surface it already
    // declined against — and the lane declines again, against a request that is
    // by then inside its own surface, which normalizes to nothing and falls
    // through to a plain park naming no remedy.
    let orders: Vec<String> = harness
        .ledger()
        .into_iter()
        .filter(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND)
        .filter_map(|run| run.task)
        .collect();
    assert!(orders.len() >= 2, "the amendment re-dispatched the member: {orders:?}");
    let declining = orders.first().expect("the member's first lap carried a work order");
    let amended = orders.last().expect("the re-armed member re-entered construct with a work order");
    assert!(!declining.contains("crates/example-b/**"), "the declining lap ran under the sealed surface: {declining}");
    assert!(
        amended.contains("crates/example-b/**"),
        "the re-dispatched lap's work order declares the amended surface: {amended}",
    );
}

#[test]
fn an_operator_amend_reaches_the_re_dispatched_lane() {
    // The operator-move entry point used to lower Amend to a bare supersede
    // and skip the work-order render, so the successor had no
    // dispatch-description row and the re-dispatched lane got a subject-only
    // prompt.
    let repo = Repo::with_example_project();
    let mut harness = HarnessBuilder::local_authority(&repo).hold_repo(repo).start("bloomery-harness");
    harness.script_lane(&WorkpieceId("wp".into()), StageId::Construct, &[LaneScript::DeclineRequestingSurface]);

    let sealed = harness.author_scope_revision("wp", &["crates/example-a/**"]);
    let bloom = harness.seal_member("wp", sealed);
    harness.run_until(|harness| harness.bloom(bloom).members[0].awaiting_surface.is_some(), 40);

    let widened = harness
        .scope_revision(sealed)
        .expect("the sealed revision is in the commission store")
        .with_widened_surface(&["crates/example-b/**".to_owned()]);
    let revision = harness.approve_widened_revision(&widened);
    let successor = match harness.apply_operator(
        bloom,
        &OperatorMove::Amend { at_tick: 0, workpiece: WorkpieceId("wp".into()), scope_revision: revision },
    ) {
        Outcome::Superseded { successor, .. } => successor,
        other => panic!("the operator amend supersedes: {other:?}"),
    };

    let mut member = harness.bloom(successor).members[0].clone();
    for _ in 0..60 {
        if member.cursor.as_ref().is_some_and(|cursor| cursor.candidate.is_some()) {
            break;
        }
        harness.tick();
        member = harness.bloom(successor).members[0].clone();
    }

    let orders: Vec<String> = harness
        .ledger()
        .into_iter()
        .filter(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND)
        .filter_map(|run| run.task)
        .collect();
    assert!(orders.len() >= 2, "the amendment re-dispatched the member: {orders:?}");
    let amended = orders.last().expect("the re-armed member re-entered construct with a work order");
    assert!(
        amended.contains("crates/example-b/**"),
        "the re-dispatched lap's work order declares the amended surface: {amended}",
    );
}

#[test]
fn a_second_operator_amend_carries_the_first_widened_revision() {
    // A second Amend used to rebuild the successor from the pre-amendment
    // spec, re-pinning the first-amended member at its original scope.
    let mut harness = BloomeryHarness::start();
    let bloom = harness.seal_members(&[("wp-a", digest(0x51)), ("wp-b", digest(0x52))]);

    let first_revision = digest(0xA1);
    let first = match harness.apply_operator(
        bloom,
        &OperatorMove::Amend { at_tick: 0, workpiece: WorkpieceId("wp-a".into()), scope_revision: first_revision },
    ) {
        Outcome::Superseded { successor, .. } => successor,
        other => panic!("the first amendment supersedes: {other:?}"),
    };

    let second = match harness.apply_operator(
        first,
        &OperatorMove::Amend { at_tick: 1, workpiece: WorkpieceId("wp-b".into()), scope_revision: digest(0xB1) },
    ) {
        Outcome::Superseded { successor, .. } => successor,
        other => panic!("the second amendment supersedes: {other:?}"),
    };

    let view = harness.bloom(second);
    let member_a = view
        .members
        .iter()
        .find(|member| member.workpiece.0 == "wp-a")
        .expect("wp-a remains a member of the successor");
    assert_eq!(
        member_a.scope_revision, first_revision,
        "the second successor still carries the first's widened revision: {view:?}"
    );
}
