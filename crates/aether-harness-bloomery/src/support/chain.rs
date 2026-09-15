//! The bring-up two scenarios about grouping across construct heads share
//! (ADR-0218 §Amendment: grouping across heads by clean merge).
//!
//! Both need the same awkward shape: two members ready to verify at the same
//! instant whose `ConstructContext` heads are *different* and one product fold
//! apart. Eager integration produces that shape constantly and it is fiddly to
//! arrange on purpose, so the arrangement lives here once rather than twice.
//!
//! The lever is the dependency edge. A member whose dependency is unmet is not
//! dispatched at the seal; it is released when the product's coverage reaches
//! its dependency, and the reducer hands it a `ConstructContext` at the head
//! *that fold produced*. So sealing `LAGGING` and `LEADING` edgeless and
//! `FOLLOWING` behind `LEADING` gives, after one fold:
//!
//! ```text
//! generation base G0 ──fold(LEADING)──▶ head H1
//!        │                                  │
//!    LAGGING constructed here          FOLLOWING constructed here
//! ```
//!
//! and both of those members reach the verification queue together, because
//! [`ScenarioHarness::hold_member_verification`] holds the service while they
//! accumulate.

use aether_bloomery::{
    BloomId, CoordinationPolicy, Digest, Fact, IntegrationHead, RedVerify, SharedRunMode, SharedRunPlan,
    SharedRunPreparation, StageId, VerificationMode,
};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use aether_chassis_bloomery::store::StoreBackend;

use super::eager::{approve_scopes, author_and_release, coordination, covers, facts, parked_lane, product_coverage};
use crate::harness::ScenarioHarness;

/// The member that verifies and folds first, moving the product head.
pub const LEADING: &str = "wp-a";
/// The member constructed on the pristine base, still authoring across the fold.
pub const LAGGING: &str = "wp-b";
/// The member released *by* that fold, and so constructed on the head it made.
pub const FOLLOWING: &str = "wp-c";

/// Where each member is allowed to write. `LEADING` and `LAGGING` share a
/// surface so a scenario can choose whether their candidates collide.
pub const LEADING_SURFACE: &str = "crates/example-shared/**";
pub const LAGGING_SURFACE: &str = "crates/example-shared/**";
pub const FOLLOWING_SURFACE: &str = "crates/example-a/**";

/// The path `FOLLOWING` authors — disjoint from the other two, always.
pub const FOLLOWING_FILE: &str = "crates/example-a/src/chain.rs";

/// Contextual verification with room for two members in one run, eager
/// integration so the head actually moves, and no coalescing wait: the
/// scenarios gate readiness themselves with the verification-service hold, and
/// the default hold would park a ready request behind the sibling construct
/// these scenarios deliberately leave in flight.
#[must_use]
pub fn grouping_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: RedVerify::Refine,
        verification: VerificationMode::Contextual,
        eager_integration: true,
        max_run_members: 2,
        max_serial_requests: 2,
        max_attribution_probes: 2,
        movement_budget: 3,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: Some(0),
    }
}

/// The lane script both scenarios run: every stage passes, and every Construct
/// parks so the scenario writes the candidate itself.
#[must_use]
pub fn parking_script() -> LaneScript {
    [LEADING, LAGGING, FOLLOWING].iter().fold(LaneScript::all_passing(), |script, workpiece| {
        script.then_for(*workpiece, StageId::Construct, LaneMode::NeverExits)
    })
}

/// Seal the three members with `FOLLOWING` waiting on `LEADING`.
///
/// # Panics
/// The seal was refused.
pub fn seal_chain(harness: &mut ScenarioHarness) -> BloomId {
    let revisions = [(LEADING, LEADING_SURFACE), (LAGGING, LAGGING_SURFACE), (FOLLOWING, FOLLOWING_SURFACE)]
        .map(|(workpiece, surface)| (workpiece, harness.author_scope_revision(workpiece, &[surface])));
    approve_scopes(harness, &revisions.iter().map(|(_, revision)| *revision).collect::<Vec<_>>());
    harness.seal_graph(&revisions, &[(FOLLOWING, LEADING)])
}

/// Drive the bloom to the instant both `LAGGING` and `FOLLOWING` are queued for
/// verification on their own — different — construct heads, and return those two
/// heads in chain order.
///
/// `lagging_file` and `lagging_contents` are what `LAGGING` authors: a scenario
/// says whether that candidate merges onto the head `LEADING`'s fold produced by
/// choosing a path and contents that do or do not collide with `leading_file`.
///
/// # Panics
/// The bloom does not reach that state inside the harness's pump budget, or the
/// two members are not on two distinct heads when it does.
pub fn queue_two_members_on_successive_heads(
    harness: &mut ScenarioHarness,
    bloom: BloomId,
    leading: (&str, &str),
    lagging: (&str, &str),
) -> (IntegrationHead, IntegrationHead) {
    // Both edgeless members author over the pristine base. `LAGGING` stays
    // parked across the fold, which is what keeps its construct head behind.
    let leading_order = parked_lane(harness, LEADING, StageId::Construct);
    let lagging_order = parked_lane(harness, LAGGING, StageId::Construct);
    let earlier = coordination(harness, bloom).contexts[LAGGING].starting_head.clone();
    assert!(earlier.coverage.is_empty(), "both edgeless members were admitted on the pristine base");

    author_and_release(harness, &leading_order, leading.0, leading.1);
    harness.pump_until("the leading member verifies and the product absorbs it", |harness| {
        harness.dispatch_tick();
        harness.integrate_tick();
        covers(&product_coverage(harness, bloom), LEADING)
    });
    let later = coordination(harness, bloom).integration.head;
    assert!(earlier.precedes(&later), "the fold moved the product one head along the chain: {earlier:?} {later:?}");

    // From here the two candidates must reach the queue together, so the
    // scheduler sees one selection rather than proposing each as it lands.
    harness.hold_member_verification(true);
    let following_order = parked_lane(harness, FOLLOWING, StageId::Construct);
    assert_eq!(
        coordination(harness, bloom).contexts[FOLLOWING].starting_head,
        later,
        "the member the fold released is constructed on the head that fold produced"
    );

    author_and_release(harness, &lagging_order, lagging.0, lagging.1);
    author_and_release(harness, &following_order, FOLLOWING_FILE, "pub fn following() -> u8 {\n    1\n}\n");
    harness.pump_until("both trailing candidates are captured and queued", |harness| {
        harness.commission_store().queued_member_verifications().expect("the verification queue reads").len() == 2
    });
    harness.hold_member_verification(false);

    (earlier, later)
}

/// Every shared-run plan the coordinator journaled a proposal for, oldest first.
#[must_use]
pub fn proposed_plans(harness: &ScenarioHarness) -> Vec<SharedRunPlan> {
    facts(harness, |fact| match fact {
        Fact::ProposeSharedRun { plan, .. } => Some(plan.clone()),
        _ => None,
    })
}

/// Every standalone plan the coordination state holds for `workpiece`.
///
/// A standalone fallback is minted inside the reducer rather than proposed by
/// the scheduler, so it never appears as a `ProposeSharedRun` fact and has to be
/// read off the coordination state the journal projects.
#[must_use]
pub fn standalone_runs(harness: &ScenarioHarness, bloom: BloomId, workpiece: &str) -> Vec<SharedRunPlan> {
    coordination(harness, bloom)
        .runs
        .into_iter()
        .map(|run| run.plan)
        .filter(|plan| {
            plan.mode == SharedRunMode::Standalone
                && plan.requests.iter().any(|request| request.member.workpiece.0 == workpiece)
        })
        .collect()
}

/// Every plan whose requests name `workpiece`, oldest first.
#[must_use]
pub fn plans_for(harness: &ScenarioHarness, workpiece: &str) -> Vec<SharedRunPlan> {
    proposed_plans(harness)
        .into_iter()
        .filter(|plan| plan.requests.iter().any(|request| request.member.workpiece.0 == workpiece))
        .collect()
}

/// How each journaled preparation answered, keyed by the plan it prepared.
#[must_use]
pub fn preparations(harness: &ScenarioHarness) -> Vec<(Digest, SharedRunPreparation)> {
    facts(harness, |fact| match fact {
        Fact::SharedRunPrepared { plan, preparation, .. } => Some((*plan, preparation.clone())),
        _ => None,
    })
}

/// The construct head a plan's request for `workpiece` names.
///
/// # Panics
/// The plan carries no request for `workpiece`.
#[must_use]
pub fn request_base(plan: &SharedRunPlan, workpiece: &str) -> Option<IntegrationHead> {
    plan.requests
        .iter()
        .find(|request| request.member.workpiece.0 == workpiece)
        .expect("the plan carries the member's request")
        .context
        .as_ref()
        .map(|context| context.starting_head.clone())
}
