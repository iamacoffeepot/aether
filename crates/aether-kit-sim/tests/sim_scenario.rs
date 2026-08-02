//! Tick-native turn simulation through two real instances of the kit-sim wasm.

use aether_harness_substrate::test_helpers::require_wasm;
use std::fs;
use std::path::Path;

use aether_actor::Addressable;
use aether_data::Kind;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_kit_sim::sim::{
    CellPosition, EntityState, GridBounds, MoveDirection, MoveIntent, Poll, PollResult, SimConfig, Spawn,
    TrajectoryEvent, TrajectoryKind,
};

#[allow(unused_imports)]
use aether_kit_sim as _;

const FIRST_SIM_NAME: &str = "turn-sim-a";
const SECOND_SIM_NAME: &str = "turn-sim-b";
const ENTITY_ID: u64 = 41;

#[test]
fn sim_vocabulary_is_the_exact_lower_crate_wire_contract() {
    use aether_game::Spawn as CapabilitySpawn;
    use aether_kit_sim::Spawn as RootSpawn;

    let lower = CapabilitySpawn { entity_id: 9, cell_x: -2, cell_z: 4 };
    let root_reexport: RootSpawn = lower;
    let sim_reexport: Spawn = root_reexport;

    assert_eq!(Spawn::NAME, "aether.sim.spawn");
    assert_eq!(Spawn::ID, CapabilitySpawn::ID);
    assert_eq!(CapabilitySpawn::decode_from_bytes(&sim_reexport.encode_into_bytes()), Some(lower));
}

fn component_address(name: &str) -> String {
    format!("aether.component/{}:{name}", aether_component::WasmTrampoline::NAMESPACE)
}

fn load_sim(harness: &mut SubstrateHarness, wasm_path: &Path, name: &str) -> String {
    let config = SimConfig {
        fact_sink: None,
        ring_depth: 8,
        grid_bounds: GridBounds { min_cell_x: -4, max_cell_x: 4, min_cell_z: -4, max_cell_z: 4 },
    };
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: fs::read(wasm_path).expect("read kit wasm for turn sim"),
                    name: Some(name.to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some("aether.kit.sim".to_owned()),
                    replica: None,
                },
            ),
        )])
        .expect("load turn sim sequence");
    match loaded.reply::<LoadResult>("load").expect("decode turn-sim LoadResult") {
        LoadResult::Ok { name: loaded_name, .. } => {
            let address = component_address(name);
            assert_eq!(loaded_name, address);
            address
        }
        LoadResult::Err { error } => panic!("load turn sim: {error}"),
    }
}

fn poll(harness: &mut SubstrateHarness, address: &str, label: &'static str) -> PollResult {
    harness
        .execute(vec![(label, HarnessOp::send_and_await_reply(address, &Poll { since_tick: 0 }))])
        .expect("poll turn sim")
        .reply::<PollResult>(label)
        .expect("decode PollResult")
}

#[test]
fn turn_sim_moves_in_tick_order_and_replays_byte_identically_through_real_wasm() {
    let Some(wasm_path) = require_wasm("aether_kit_sim") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(96, 96).with_component_host().build().expect("boot");
    let first = load_sim(&mut harness, &wasm_path, FIRST_SIM_NAME);
    let second = load_sim(&mut harness, &wasm_path, SECOND_SIM_NAME);

    harness
        .execute(vec![
            ("spawn-a", HarnessOp::send_and_settle(&first, &Spawn { entity_id: ENTITY_ID, cell_x: 1, cell_z: 1 })),
            ("spawn-b", HarnessOp::send_and_settle(&second, &Spawn { entity_id: ENTITY_ID, cell_x: 1, cell_z: 1 })),
            ("spawn-turn", HarnessOp::advance(1)),
            (
                "superseded-east-a",
                HarnessOp::send_and_settle(
                    &first,
                    &MoveIntent { entity_id: ENTITY_ID, direction: MoveDirection::East },
                ),
            ),
            (
                "winning-north-a",
                HarnessOp::send_and_settle(
                    &first,
                    &MoveIntent { entity_id: ENTITY_ID, direction: MoveDirection::North },
                ),
            ),
            (
                "superseded-east-b",
                HarnessOp::send_and_settle(
                    &second,
                    &MoveIntent { entity_id: ENTITY_ID, direction: MoveDirection::East },
                ),
            ),
            (
                "winning-north-b",
                HarnessOp::send_and_settle(
                    &second,
                    &MoveIntent { entity_id: ENTITY_ID, direction: MoveDirection::North },
                ),
            ),
            ("move-and-following-turns", HarnessOp::advance(3)),
        ])
        .expect("drive identical turn-sim sequences");

    let first_result = poll(&mut harness, &first, "poll-a");
    let second_result = poll(&mut harness, &second, "poll-b");

    assert_eq!(first_result.current_tick, 4);
    assert_eq!(
        first_result.bundles.iter().map(|bundle| bundle.tick).collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "one HarnessOp::advance tick must produce one ordered turn bundle"
    );
    let moved = &first_result.bundles[1];
    assert_eq!(moved.tick, 2);
    assert_eq!(moved.superseded_through, 2, "the summary supersedes trajectory through its own tick");
    assert_eq!(
        moved.trajectory,
        vec![TrajectoryEvent {
            tick: 2,
            entity_id: ENTITY_ID,
            kind: TrajectoryKind::Moved {
                from: CellPosition { cell_x: 1, cell_z: 1 },
                to: CellPosition { cell_x: 1, cell_z: 0 },
            },
        }],
        "the later north intent must win the tick bin over east"
    );
    assert_eq!(
        moved.summary.entities,
        vec![EntityState { entity_id: ENTITY_ID, cell_x: 1, cell_z: 0 }],
        "the post-turn summary must carry the trajectory endpoint"
    );

    assert_eq!(first_result.current_tick, second_result.current_tick);
    assert_eq!(first_result.bundles.len(), second_result.bundles.len());
    for (first_bundle, second_bundle) in first_result.bundles.iter().zip(&second_result.bundles) {
        assert_eq!(
            first_bundle.encode_into_bytes(),
            second_bundle.encode_into_bytes(),
            "identical state and intent sequences must emit byte-identical tick bundles"
        );
    }
}
