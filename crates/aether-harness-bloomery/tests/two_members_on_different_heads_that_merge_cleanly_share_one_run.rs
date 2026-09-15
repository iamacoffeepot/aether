//! Two members constructed on different heads of one product chain verify in
//! ONE shared run over the newer of those heads, and both fold out of it
//! (ADR-0218 §Amendment: grouping across heads by clean merge).
//!
//! `wp-b` is admitted on the pristine base and parks there while `wp-a` verifies
//! and folds; `wp-c` is released *by* that fold and is therefore constructed on
//! the head it produced. Both reach the verification queue together, one head
//! apart. Before this change the selector admitted a request into a contextual
//! group only when its `context.starting_head` equalled the first selected
//! request's, so this pair — which is the ordinary shape under eager
//! integration, not a corner — was planned as two runs. Bloom `0c5a` is the
//! measurement: `shared_run_members` held one row per run for nearly the whole
//! board, so the shared path bought nothing at all.
//!
//! The pair is admitted because the bases are on one chain and both candidates
//! merge onto the newer one. Nothing here re-prepares the lagging member: its
//! request is byte-identical to the one the fold left alone, still naming its
//! own construct head, and the composition it joins carries that head forward
//! rather than replacing it.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{Fact, MemberVerifyRequest, SharedRunMode};
use aether_harness_bloomery::support::chain::{
    FOLLOWING, LAGGING, LEADING, grouping_policy, parking_script, plans_for, proposed_plans,
    queue_two_members_on_successive_heads, request_base, seal_chain,
};
use aether_harness_bloomery::support::eager::{covers, facts, product_coverage};
use aether_harness_bloomery::{HarnessBuilder, Repo};

const LEADING_FILE: &str = "crates/example-shared/src/leading.rs";
const LAGGING_FILE: &str = "crates/example-shared/src/lagging.rs";

#[test]
fn two_members_on_different_heads_that_merge_cleanly_share_one_run() {
    let authority = Repo::with_formatted_example_project();
    let script = parking_script();
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(grouping_policy())
        .script(&script)
        // Two members prove at once here, and the base verify takes a prove slot
        // of its own; the auto ceiling is one per eight host cores, which on a
        // small runner is one.
        .max_concurrent_provers(2)
        .start("two-members-on-different-heads-share-one-run");
    let bloom = seal_chain(&mut harness);

    let (earlier, later) = queue_two_members_on_successive_heads(
        &mut harness,
        bloom,
        (LEADING_FILE, "pub fn leading() -> u8 {\n    1\n}\n"),
        (LAGGING_FILE, "pub fn lagging() -> u8 {\n    1\n}\n"),
    );

    harness.pump_until("the two trailing members are planned", |harness| {
        harness.dispatch_tick();
        !plans_for(harness, LAGGING).is_empty() && !plans_for(harness, FOLLOWING).is_empty()
    });
    let lagging_plans = plans_for(&harness, LAGGING);
    assert_eq!(lagging_plans.len(), 1, "the lagging member is planned once: {lagging_plans:?}");
    let plan = lagging_plans.into_iter().next().expect("the lagging member was planned");

    assert_eq!(
        plan.requests.iter().map(|request| request.member.workpiece.0.clone()).collect::<Vec<_>>(),
        vec![LAGGING.to_owned(), FOLLOWING.to_owned()],
        "the two members built one head apart are in one run, not two"
    );
    assert_eq!(plan.mode, SharedRunMode::Contextual);
    let composition = plan.composition.as_ref().expect("a contextual plan carries its composition");
    assert_eq!(composition.base, later, "the group stands on the newest construct base among its members");
    assert!(covers(&composition.base.coverage, LEADING), "which is the head the leading member's fold produced");
    assert_eq!(request_base(&plan, LAGGING), Some(earlier), "the lagging member's request still names its own head");
    assert_eq!(request_base(&plan, FOLLOWING), Some(later));
    assert_eq!(
        composition.admissions().iter().map(|admission| admission.carried_forward).collect::<Vec<_>>(),
        vec![true, false],
        "and the plan says which of the two was carried forward, so a console can show why each is here"
    );

    harness.pump_until("the shared run completes", |harness| {
        harness.dispatch_tick();
        facts(harness, |fact| match fact {
            Fact::SharedRunCompleted { completion, .. } if completion.plan == plan.digest() => Some(()),
            _ => None,
        })
        .len()
            == 1
    });
    assert_eq!(
        facts(&harness, |fact| match fact {
            Fact::SharedRunCompleted { completion, .. } => Some(completion.plan),
            _ => None,
        })
        .len(),
        2,
        "three members were proved by two physical runs: the leading one alone, the other two together"
    );

    harness.pump_until("both members of the shared run fold onto the product", |harness| {
        harness.dispatch_tick();
        harness.integrate_tick();
        let coverage = product_coverage(harness, bloom);
        covers(&coverage, LAGGING) && covers(&coverage, FOLLOWING)
    });
    let settled = product_coverage(&harness, bloom);
    assert!(covers(&settled, LEADING), "the product carries all three members: {settled:?}");

    let requests = proposed_plans(&harness)
        .iter()
        .flat_map(|plan| plan.requests.iter().map(MemberVerifyRequest::digest).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 3, "no member was re-planned: each was proposed exactly once: {requests:?}");
}
