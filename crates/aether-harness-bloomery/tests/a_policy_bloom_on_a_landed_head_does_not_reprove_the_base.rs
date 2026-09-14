//! A policy bloom sealed on a head the previous bloom just landed must not
//! re-dispatch `verify.base`.
//!
//! Pre-fix: `enqueue_base_verify_if_needed` treated a coordinated seal as
//! unproven unless the green receipt spelled `tree == base`, which is only how
//! a host `verify.base` run records the checkout. A landing files the fold
//! tree, so every policy bloom after the first of the day waited a
//! whole-workspace run for a head the landing had already proven.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    BloomDraft, CoordinationPolicy, Decision, Digest, Evidence, EvidenceKind, Fact, Outcome, StageId, VerificationMode,
    VerifyFailureSet, config_address, decode_recorded_decisions, decode_recorded_event,
};
use aether_chassis_bloomery::store::{OutstandingOrder, StoreBackend};
use aether_data::Kind;
use aether_data::wire::{from_bytes, to_vec};
use aether_harness_bloomery::{FixtureHarness, ScenarioHarness, captured, digest, member, passed};

const FIRST: &str = "wp-0";
const SECOND: &str = "wp-1";

fn policy() -> CoordinationPolicy {
    CoordinationPolicy {
        verification: VerificationMode::Standalone,
        eager_integration: false,
        max_run_members: 1,
        max_serial_requests: 1,
        max_attribution_probes: 0,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: None,
    }
}

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

fn seal_effects(harness: &ScenarioHarness) -> Vec<Vec<Decision>> {
    harness
        .commission_store()
        .replay_journal()
        .expect("the coordinator journal replays")
        .into_iter()
        .filter_map(|row| {
            let event = decode_recorded_event(&row.event, row.event_schema.as_deref()).ok()?;
            matches!(event.fact, Fact::Seal(_)).then(|| {
                decode_recorded_decisions(&row.decisions, row.decisions_schema_digest.as_deref())
                    .expect("the seal's decisions decode")
                    .effects
            })
        })
        .collect()
}

fn dispatched_base_verify(effects: &[Decision]) -> bool {
    effects.iter().any(|effect| matches!(effect, Decision::DispatchBaseVerify { .. }))
}

fn queued_construct(effects: &[Decision]) -> bool {
    effects.iter().any(|effect| matches!(effect, Decision::QueueConstructionAdmission { .. }))
}

fn record_policy(harness: &ScenarioHarness, policy: &CoordinationPolicy) -> Digest {
    let bytes = to_vec(policy).expect("the coordination policy encodes");
    let address = config_address(CoordinationPolicy::NAME, &bytes);
    harness
        .commission_store()
        .record_config(address.as_bytes(), CoordinationPolicy::NAME, &bytes)
        .expect("the coordination policy records");
    address
}

#[test]
fn a_policy_bloom_on_a_landed_head_does_not_reprove_the_base() {
    let mut harness = FixtureHarness::start("policy-bloom-on-a-landed-head");
    let (bloom, outcome) = harness.try_seal(&[(FIRST, digest(0x51))]);
    assert!(matches!(outcome, Outcome::Sealed(_)), "got {outcome:?}");

    let base = harness.await_order();
    assert_eq!(stage_of(&base), StageId::BaseVerify);
    assert!(
        harness.orders().iter().all(|order| stage_of(order) != StageId::Construct),
        "an unproven head withholds construct",
    );
    harness.upload_admitted(&passed(&base));

    let construct = harness.await_order();
    assert_eq!(stage_of(&construct), StageId::Construct);
    let candidate = harness.seed_capture(bloom, FIRST, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, candidate));

    let verify = harness.await_order();
    assert_eq!(verify.workpiece, FIRST);
    harness.upload_admitted(&passed(&verify));
    harness.land_the_fold(bloom);

    // A host `verify.base` run records the checkout in both fields. Coordinated
    // seals used to treat that spelling as unproven and refresh; it is already
    // the tree this head resolves to.
    let head = harness.view().mainline;
    match harness.admit(
        "host-tree-as-base",
        Fact::BaseVerifyCompleted {
            base: head,
            tree: head,
            passed: true,
            evidence: Evidence { subject: head, kind: EvidenceKind::VerificationResult, detail: digest(0x90) },
            failed: VerifyFailureSet::EMPTY,
        },
    ) {
        Outcome::BaseProven { .. } => {}
        other => panic!("the landed head must take a host tree:base receipt: {other:?}"),
    }

    let address = record_policy(&harness, &policy());
    let template = harness.successor_draft(&[member(SECOND, digest(0x52))]);
    let mut configs = template.configs().clone();
    configs.insert::<CoordinationPolicy>(address);
    let spec = BloomDraft {
        proposals: template.members().to_vec(),
        base: template.base(),
        configs,
        forecast: template.forecast(),
    }
    .seal();
    let next = spec.id();
    match harness.admit("policy-seal-on-landed-head", Fact::Seal(spec)) {
        Outcome::Sealed(sealed) => assert_eq!(sealed, next),
        other => panic!("a policy bloom on a landed head must seal: {other:?}"),
    }

    let second_seal = seal_effects(&harness).pop().expect("the policy seal journals");
    assert!(
        !dispatched_base_verify(&second_seal),
        "a policy bloom on a landed head must not re-prove the base: {second_seal:?}"
    );
    assert!(
        queued_construct(&second_seal),
        "the landed head's receipt admits construct in the same decision set: {second_seal:?}"
    );
    assert!(
        harness.orders().iter().all(|order| stage_of(order) != StageId::BaseVerify),
        "the landed head's receipt is the proof the next seal stands on",
    );
}
