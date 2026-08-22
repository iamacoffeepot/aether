//! Generated coordinator scenarios. The bug class this suite exists to
//! catch: a generated scenario reaches a state where a member has no lane,
//! no outstanding order, no wedge, and no named park.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::env;

use aether_bloomery::{StageId, WorkpieceId};
use aether_harness_bloomery::{BloomeryHarness, LaneScript, MemberSpec, Scenario};
use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;

fn lane_script_strategy() -> impl Strategy<Value = LaneScript> {
    prop_oneof![
        Just(LaneScript::Candidate),
        Just(LaneScript::Decline),
        Just(LaneScript::Die),
        Just(LaneScript::WrongSubject),
    ]
}

fn scenario_strategy() -> impl Strategy<Value = Scenario> {
    lane_script_strategy().prop_map(|script| {
        let mut scripts = BTreeMap::new();
        scripts.insert(StageId::Construct, vec![script]);
        Scenario {
            members: vec![MemberSpec {
                workpiece: WorkpieceId("wp".into()),
                surface: vec!["crates/example-a/**".into()],
                script: scripts,
            }],
            collide: Vec::new(),
            supersede: None,
            operator: Vec::new(),
        }
    })
}

fn case_count() -> u32 {
    env::vars().find(|(key, _)| key == "BLOOMERY_HARNESS_CASES").and_then(|(_, value)| value.parse().ok()).unwrap_or(4)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: case_count(), ..ProptestConfig::default() })]

    #[test]
    fn a_generated_scenario_never_silences_a_member(scenario in scenario_strategy()) {
        let mut harness = BloomeryHarness::start();
        for spec in &scenario.members {
            if let Some(scripts) = spec.script.get(&StageId::Construct) {
                harness.script_lane(&spec.workpiece, StageId::Construct, scripts);
            }
        }
        let bloom = harness.seal_scenario(&scenario);
        harness.run_until(
            |harness| {
                harness.bloom(bloom).members.iter().all(|member| {
                    member.resolution.is_some()
                        || member.wedge.is_some()
                        || member.host_fault.is_some()
                        || member.park.is_some()
                })
            },
            60,
        );
    }
}
