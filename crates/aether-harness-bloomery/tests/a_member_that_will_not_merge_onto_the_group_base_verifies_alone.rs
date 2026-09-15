//! The other half of grouping across heads: when one of the two candidates will
//! not merge onto the newer base, it is dropped from the group and verified
//! alone over **its own** construct base — never re-prepared onto the head that
//! moved under it, and never sent to reconcile from the scheduler
//! (ADR-0218 §Amendment: grouping across heads by clean merge).
//!
//! Same shape as the clean scenario, with one change: `wp-b` authors the exact
//! path `wp-a` folded, with different contents. `wp-b`'s base is the pristine
//! tree, which carries neither version, so placing its candidate onto the head
//! `wp-a`'s fold produced is an add/add collision — and preparation, which is
//! where the source already merges a composition, is what discovers it.
//!
//! What must then happen is what happened before this change and must keep
//! happening: the composition says nothing about either member's own candidate
//! (none of them has been verified yet), so each falls back to proving the tree
//! it authored over the base it authored it on. The conflict is real, and it is
//! rediscovered at the fold — on a member that is green by then and can
//! reconcile the merge alone.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{Fact, SharedRunPreparation, StageId};
use aether_harness_bloomery::support::chain::{
    FOLLOWING, LAGGING, LEADING, grouping_policy, parking_script, plans_for, preparations,
    queue_two_members_on_successive_heads, request_base, seal_chain, standalone_runs,
};
use aether_harness_bloomery::support::eager::{covers, facts, product_coverage, stage_of};
use aether_harness_bloomery::{HarnessBuilder, Repo};

/// The one file both `wp-a` and `wp-b` write, each with its own contents. Both
/// surfaces cover it, so neither member is out of surface — the trees simply do
/// not merge.
const COLLIDING_FILE: &str = "crates/example-shared/src/collide.rs";

#[test]
fn a_member_that_will_not_merge_onto_the_group_base_verifies_alone() {
    let authority = Repo::with_formatted_example_project();
    let script = parking_script();
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(grouping_policy())
        .script(&script)
        // Two members prove at once here, and the base verify takes a prove slot
        // of its own; the auto ceiling is one per eight host cores, which on a
        // small runner is one.
        .max_concurrent_provers(2)
        .start("a-member-that-will-not-merge-verifies-alone");
    let bloom = seal_chain(&mut harness);

    let (earlier, later) = queue_two_members_on_successive_heads(
        &mut harness,
        bloom,
        (COLLIDING_FILE, "pub fn collide() -> u8 {\n    1\n}\n"),
        (COLLIDING_FILE, "pub fn collide() -> &'static str {\n    \"other\"\n}\n"),
    );

    harness.pump_until("the optimistic group is planned and prepared", |harness| {
        harness.dispatch_tick();
        plans_for(harness, LAGGING).iter().any(|plan| plan.requests.len() == 2)
            && preparations(harness)
                .iter()
                .any(|(_, preparation)| matches!(preparation, SharedRunPreparation::Conflict { .. }))
    });
    let group = plans_for(&harness, LAGGING)
        .into_iter()
        .find(|plan| plan.requests.len() == 2)
        .expect("the two members were grouped optimistically");
    let (_, conflict) = preparations(&harness)
        .into_iter()
        .find(|(plan, _)| *plan == group.digest())
        .expect("the group's preparation was journaled");
    let SharedRunPreparation::Conflict { input, .. } = conflict else {
        panic!("the group's placement must collide: {conflict:?}");
    };
    assert_eq!(
        input.members.iter().map(|pin| pin.workpiece.0.clone()).collect::<Vec<_>>(),
        vec![LAGGING.to_owned()],
        "the conflict names the candidate that would not merge onto the newer base"
    );

    // Each member drops back to its own base. The fallback is minted in the
    // reducer rather than proposed, so it is read off the coordination state.
    harness.pump_until("both members fall back to a run of their own", |harness| {
        [LAGGING, FOLLOWING].iter().all(|workpiece| !standalone_runs(harness, bloom, workpiece).is_empty())
    });
    for (workpiece, base) in [(LAGGING, &earlier), (FOLLOWING, &later)] {
        let alone = standalone_runs(&harness, bloom, workpiece);
        assert_eq!(alone.len(), 1, "{workpiece} falls back to exactly one run of its own: {alone:?}");
        let alone = alone.into_iter().next().expect("the fallback run");
        assert!(alone.composition.is_none(), "a standalone fallback composes nothing: {alone:?}");
        assert_eq!(
            request_base(&alone, workpiece).as_ref(),
            Some(base),
            "{workpiece} is proved over the base it was constructed on, unchanged by the group it left"
        );
        assert_eq!(
            alone.requests,
            group
                .requests
                .iter()
                .filter(|request| request.member.workpiece.0 == workpiece)
                .cloned()
                .collect::<Vec<_>>(),
            "{workpiece}'s request is the one the group carried, byte for byte: nothing was re-prepared"
        );
    }

    // The conflicting member still verifies its own candidate — the collision
    // said nothing about that tree — and only then does the fold meet the merge,
    // on a member that is green by the time it is asked about it.
    harness.pump_until("the conflicting member proves its own tree and the fold answers the merge", |harness| {
        covers(&product_coverage(harness, bloom), FOLLOWING)
            && harness.orders().iter().any(|order| stage_of(order) == StageId::Reconcile && order.workpiece == LAGGING)
    });
    assert_eq!(
        facts(&harness, |fact| match fact {
            Fact::SharedRunCompleted { completion, .. } => Some(completion.plan),
            _ => None,
        })
        .into_iter()
        .filter(|plan| standalone_runs(&harness, bloom, LAGGING).iter().any(|alone| alone.digest() == *plan))
        .count(),
        1,
        "the dropped member's own standalone run completed before the merge was ever asked about"
    );
    assert!(covers(&product_coverage(&harness, bloom), LEADING), "the leading member's fold is untouched by any of it");
}
