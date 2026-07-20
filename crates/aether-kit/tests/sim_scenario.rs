//! Tick-native turn simulation through two real instances of the kit wasm.

use aether_substrate_bundle::FullBenchExt;
use std::fs;
use std::path::Path;

use aether_actor::Addressable;
use aether_data::Kind;
use aether_kinds::{LoadComponent, LoadResult};
use aether_kit::sim::{
    CellPosition, EntityState, GridBounds, MoveDirection, MoveIntent, Poll, PollResult, SimConfig, Spawn,
    TrajectoryEvent, TrajectoryKind,
};
use aether_substrate_bench::{BenchOp, SubstrateBench};
use aether_substrate_bench_capture::test_helpers::require_runtime;

#[allow(unused_imports)]
use aether_kit as _;

const FIRST_SIM_NAME: &str = "turn-sim-a";
const SECOND_SIM_NAME: &str = "turn-sim-b";
const ENTITY_ID: u64 = 41;

#[test]
fn sim_vocabulary_is_the_exact_lower_crate_wire_contract() {
    use aether_game::Spawn as CapabilitySpawn;
    use aether_kit::Spawn as RootSpawn;

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

fn load_sim(bench: &mut SubstrateBench, wasm_path: &Path, name: &str) -> String {
    let config = SimConfig {
        fact_sink: None,
        ring_depth: 8,
        grid_bounds: GridBounds { min_cell_x: -4, max_cell_x: 4, min_cell_z: -4, max_cell_z: 4 },
    };
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: fs::read(wasm_path).expect("read kit wasm for turn sim"),
                    name: Some(name.to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some("aether.kit.sim".to_owned()),
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

fn poll(bench: &mut SubstrateBench, address: &str, label: &'static str) -> PollResult {
    bench
        .execute(vec![(label, BenchOp::send_and_await(address, &Poll { since_tick: 0 }))])
        .expect("poll turn sim")
        .reply::<PollResult>(label)
        .expect("decode PollResult")
}

#[test]
fn turn_sim_moves_in_tick_order_and_replays_byte_identically_through_real_wasm() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let mut bench = SubstrateBench::builder().size(96, 96).full().build().expect("boot");
    let first = load_sim(&mut bench, &wasm_path, FIRST_SIM_NAME);
    let second = load_sim(&mut bench, &wasm_path, SECOND_SIM_NAME);

    bench
        .execute(vec![
            ("spawn-a", BenchOp::send_mail(&first, &Spawn { entity_id: ENTITY_ID, cell_x: 1, cell_z: 1 })),
            ("spawn-b", BenchOp::send_mail(&second, &Spawn { entity_id: ENTITY_ID, cell_x: 1, cell_z: 1 })),
            ("spawn-turn", BenchOp::advance(1)),
            (
                "superseded-east-a",
                BenchOp::send_mail(&first, &MoveIntent { entity_id: ENTITY_ID, direction: MoveDirection::East }),
            ),
            (
                "winning-north-a",
                BenchOp::send_mail(&first, &MoveIntent { entity_id: ENTITY_ID, direction: MoveDirection::North }),
            ),
            (
                "superseded-east-b",
                BenchOp::send_mail(&second, &MoveIntent { entity_id: ENTITY_ID, direction: MoveDirection::East }),
            ),
            (
                "winning-north-b",
                BenchOp::send_mail(&second, &MoveIntent { entity_id: ENTITY_ID, direction: MoveDirection::North }),
            ),
            ("move-and-following-turns", BenchOp::advance(3)),
        ])
        .expect("drive identical turn-sim sequences");

    let first_result = poll(&mut bench, &first, "poll-a");
    let second_result = poll(&mut bench, &second, "poll-b");

    assert_eq!(first_result.current_tick, 4);
    assert_eq!(
        first_result.bundles.iter().map(|bundle| bundle.tick).collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "one BenchOp::advance tick must produce one ordered turn bundle"
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
