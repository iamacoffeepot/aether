//! Contextual verify must show up on the members who ran, with an end on each
//! span and gate substages that sum to the run. Pre-fix every verify bar sat
//! on the composition workpiece and had only a start, so duration was whatever
//! the next span implied and `shared_run_steps.duration_millis` stayed 0.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    CoordinationPolicy, Digest, FakeKeyProvider, KeyId, MetricsLedger, ResolvedConfigs, SPAN_SUBSTAGE_PREPARE,
    Snapshot, StageId, TimelineGateTiming, TimelineTimings, VerificationMode, WorkpieceId, decode_recorded_decisions,
    decode_recorded_event, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::LaneScript;
use aether_chassis_bloomery::store::{CommissionBackend, SharedRunLifecycle, StoreBackend};
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";

fn contextual_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Refine,
        verification: VerificationMode::Contextual,
        eager_integration: true,
        max_run_members: 1,
        max_serial_requests: 1,
        max_attribution_probes: 2,
        movement_budget: 2,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: None,
    }
}

fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("verify-span harness")), &[0x0A; 32], *scope_revision);
        store.insert_approval(&approval, &FakeKeyProvider).expect("the member scope retains its signed approval");
    }
}

fn replay_metrics(store: &mut dyn StoreBackend) -> MetricsLedger {
    let mut snapshot = Snapshot::default();
    let mut ledger = MetricsLedger::default();
    let configs = ResolvedConfigs::default();
    for row in store.replay_journal().expect("the coordinator journal replays") {
        let event =
            decode_recorded_event(&row.event, row.event_schema.as_deref()).expect("the harness journal event decodes");
        let decisions = decode_recorded_decisions(&row.decisions, row.decisions_schema_digest.as_deref())
            .expect("the harness journal decisions decode");
        ledger.observe(row.sequence, &event, &decisions, &configs, row.recorded_unix_millis);
        snapshot = snapshot.apply(&event, &decisions, &configs);
    }
    ledger
}

#[test]
fn a_contextual_verify_is_attributed_to_members_with_substage_spans() {
    let authority = Repo::with_formatted_example_project();
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(contextual_policy())
        .script(&LaneScript::all_passing())
        .start("contextual-verify-member-spans");
    let first_scope = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
    let second_scope = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
    approve_scopes(&harness, &[first_scope, second_scope]);
    let bloom = harness.seal_members(&[(FIRST, first_scope), (SECOND, second_scope)]);

    harness.pump_until("both members have a completed shared run", |harness| {
        let mut store = harness.commission_store();
        let completed = store
            .list_shared_runs()
            .ok()
            .into_iter()
            .flatten()
            .filter(|run| run.lifecycle == SharedRunLifecycle::Completed)
            .count();
        completed >= 2
    });

    let mut store = harness.commission_store();
    let steps = store
        .list_shared_runs()
        .expect("shared runs read")
        .into_iter()
        .flat_map(|run| store.shared_run_steps(&run.run).unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(
        steps.iter().any(|step| step.duration_millis.is_some_and(|millis| millis > 0)),
        "a completed verify step records its wall-clock duration: {steps:?}"
    );

    let ledger = replay_metrics(&mut store);
    let timings = TimelineTimings {
        duration_millis: 90,
        gates: vec![
            TimelineGateTiming { command: "verify.fmt".into(), duration_millis: 10, prepare_millis: None },
            TimelineGateTiming { command: "verify.test".into(), duration_millis: 80, prepare_millis: None },
        ],
    };
    let timeline = ledger.timeline_with(bloom, |_| Some(timings.clone()));
    assert!(
        timeline.spans.iter().all(|span| span.stage != StageId::Verify || span.workpiece != WorkpieceId::COMPOSITION),
        "contextual verify is not a composition bar: {:?}",
        timeline.spans
    );
    for member in [FIRST, SECOND] {
        let runs: Vec<_> = timeline
            .spans
            .iter()
            .filter(|span| span.stage == StageId::Verify && span.workpiece == member && span.substage.is_none())
            .collect();
        assert!(!runs.is_empty(), "{member} has at least one verify span: {:?}", timeline.spans);
        assert!(
            runs.iter().all(|span| span.ended_unix_millis.is_some() && span.run.is_some()),
            "{member} verify spans carry a run id and an end: {runs:?}"
        );
        if let Some(integrated) =
            runs.iter().find(|span| span.outcome.as_deref() == Some("integrated")).or_else(|| runs.last())
        {
            let gate_millis: u64 = timeline
                .spans
                .iter()
                .filter(|span| {
                    span.run == integrated.run
                        && span.workpiece == member
                        && span.substage.as_deref().is_some_and(|name| name.starts_with("verify."))
                })
                .map(|span| span.ended_unix_millis.unwrap_or(0).saturating_sub(span.started_unix_millis.unwrap_or(0)))
                .sum();
            let duration =
                integrated.ended_unix_millis.unwrap_or(0).saturating_sub(integrated.started_unix_millis.unwrap_or(0));
            assert_eq!(gate_millis, 90, "mock gates sum to the fixture run: {member} {integrated:?}");
            assert!(
                duration == 0 || gate_millis <= duration || duration <= 90,
                "gate substages sit inside the run: gate_millis={gate_millis} duration={duration}"
            );
        }
    }
    assert!(
        timeline.spans.iter().any(|span| span.substage.as_deref() == Some(SPAN_SUBSTAGE_PREPARE)),
        "source preparation is a verify substage: {:?}",
        timeline.spans
    );
    let _ = harness.bloom(bloom);
}
