//! ADR-0230 resolution coverage for issue #6269.
//!
//! The scenario loads a resolve probe and one peer target beneath a shared
//! parent, then sends the probe the loaded target's name and an unloaded
//! name. Only the loaded name resolves, so the probe emits exactly one
//! observation.

use std::fs;

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_test_fixtures_kinds::{ResolvePeer, TickObserved};

const PROBE_EXPORT: &str = "test.probe";
const TARGET_EXPORT: &str = "test.parent_peer.target";
const RESOLVE_PROBE_EXPORT: &str = "test.resolve_probe";
const UNLOADED_NAME: &str = "test.parent_peer.missing";

fn load(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    label: &str,
    parent: Option<&str>,
    name: Option<&str>,
    export: &str,
) -> String {
    let component = LoadComponent {
        wasm: wasm.to_vec(),
        name: name.map(str::to_owned),
        config: Vec::new(),
        export: Some(export.to_owned()),
    };
    let operation = match parent {
        Some(parent) => HarnessOp::load_component_under(parent, component),
        None => HarnessOp::send_and_await_reply(ComponentHostCapability::NAMESPACE, &component),
    };
    let result = harness.execute(vec![(label, operation)]).expect("component load operation");

    match result.reply::<LoadResult>(label).expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => name,
        LoadResult::Err { error } => panic!("load {export} beneath {parent:?} failed: {error}"),
    }
}

#[test]
fn loaded_peer_resolves_and_unloaded_name_does_not() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    let outer = load(&mut harness, &wasm, "outer", None, Some("outer"), PROBE_EXPORT);
    load(&mut harness, &wasm, "target", Some(&outer), None, TARGET_EXPORT);
    let probe = load(&mut harness, &wasm, "probe", Some(&outer), None, RESOLVE_PROBE_EXPORT);

    let baseline = harness.count_observed(TickObserved::NAME);
    harness
        .execute(vec![
            ("loaded", HarnessOp::send_and_settle(&probe, &ResolvePeer { name: TARGET_EXPORT.to_owned() })),
            ("unloaded", HarnessOp::send_and_settle(&probe, &ResolvePeer { name: UNLOADED_NAME.to_owned() })),
        ])
        .expect("both resolve probes settle");
    assert_eq!(
        harness.count_observed(TickObserved::NAME) - baseline,
        1,
        "the loaded peer resolves and the unloaded name does not; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}
