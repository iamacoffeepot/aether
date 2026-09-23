//! Public `SubstrateHarness` placement coverage for issue #4535.
//!
//! The scenario uses only `HarnessOp` plus ordinary `LoadComponent` values to
//! build two component peer scopes. The fixture caller's real bare-type
//! `ctx.actor::<R>()` send proves the runtime parent selected during explicit
//! placement is what embedded resolution consumes.

use std::fs;

use aether_actor::{ActorRef, EMBEDDED_SCOPE};
use aether_component::{ComponentHostCapability, WasmTrampoline};
use aether_data::{Kind, LoadName};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_test_fixtures_kinds::{Bump, TickObserved};

const PROBE_EXPORT: &str = "test.probe";
const CALLER_EXPORT: &str = "test.parent_peer.caller";
const TARGET_EXPORT: &str = "test.parent_peer.target";

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
    let host = harness.actor_ref::<ComponentHostCapability>();
    let operation = match parent {
        Some(parent) => HarnessOp::load_component_under(&host, parent, component),
        None => HarnessOp::send_and_await_reply(&host, &component),
    };
    let result = harness.execute(vec![(label, operation)]).expect("component load operation");

    match result.reply::<LoadResult>(label).expect("decode LoadResult") {
        LoadResult::Ok { path, .. } => path.to_string(),
        LoadResult::Err { error } => panic!("load {export} beneath {parent:?} failed: {error}"),
    }
}

/// The loaded trampoline keyed `name` beneath the trampoline `parent` — the
/// placement a load beneath a component parent produces.
fn nested_trampoline(
    harness: &SubstrateHarness,
    parent: ActorRef<WasmTrampoline>,
    name: &str,
) -> ActorRef<WasmTrampoline> {
    harness
        .child::<WasmTrampoline, WasmTrampoline>(&parent, LoadName::new(name).expect("a valid load name"))
        .unwrap_or_else(|error| panic!("the trampoline loaded as {name} is live: {error}"))
}

fn assert_child_identity(loaded: &str, parent: &str, subname: &str) {
    let expected = format!("{parent}/{EMBEDDED_SCOPE}:{subname}");
    assert_eq!(loaded, expected, "LoadResult must return the registry-canonical child path");
}

#[test]
fn explicit_and_nested_parents_scope_live_peer_delivery() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    let outer = load(&mut harness, &wasm, "outer", None, Some("outer"), PROBE_EXPORT);
    let outer_target = load(&mut harness, &wasm, "outer-target", Some(&outer), None, TARGET_EXPORT);
    let outer_caller = load(&mut harness, &wasm, "outer-caller", Some(&outer), None, CALLER_EXPORT);
    assert_child_identity(&outer_target, &outer, TARGET_EXPORT);
    assert_child_identity(&outer_caller, &outer, CALLER_EXPORT);

    let nested = load(&mut harness, &wasm, "nested", Some(&outer), Some("nested"), PROBE_EXPORT);
    assert_child_identity(&nested, &outer, "nested");
    let nested_target = load(&mut harness, &wasm, "nested-target", Some(&nested), None, TARGET_EXPORT);
    let nested_caller = load(&mut harness, &wasm, "nested-caller", Some(&nested), None, CALLER_EXPORT);
    assert_child_identity(&nested_target, &nested, TARGET_EXPORT);
    assert_child_identity(&nested_caller, &nested, CALLER_EXPORT);

    let host = harness.actor_ref::<ComponentHostCapability>();
    let outer = harness
        .child::<ComponentHostCapability, WasmTrampoline>(&host, LoadName::new("outer").expect("a valid load name"))
        .expect("the outer trampoline is live");
    let outer_caller = nested_trampoline(&harness, outer, CALLER_EXPORT);
    let nested_caller = nested_trampoline(&harness, nested_trampoline(&harness, outer, "nested"), CALLER_EXPORT);

    let baseline = harness.count_observed(TickObserved::NAME);
    harness
        .execute(vec![
            ("outer-peer", HarnessOp::send_and_settle(outer_caller.erase(), &Bump)),
            ("nested-peer", HarnessOp::send_and_settle(nested_caller.erase(), &Bump)),
        ])
        .expect("both parent-relative peer sends settle");
    assert_eq!(
        harness.count_observed(TickObserved::NAME) - baseline,
        2,
        "each caller must reach the target beneath its own runtime parent; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

#[test]
fn unresolved_explicit_parent_is_a_clean_load_error() {
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let host = harness.actor_ref::<ComponentHostCapability>();
    let result = harness
        .execute(vec![(
            "missing-parent",
            HarnessOp::load_component_under(
                &host,
                "aether.component/aether.embedded:missing",
                LoadComponent { wasm: Vec::new(), name: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("the component host replies to an unresolved parent");

    let LoadResult::Err { error } = result.reply::<LoadResult>("missing-parent").expect("decode LoadResult") else {
        panic!("an unresolved logical parent must not load a component");
    };
    assert!(error.contains("component parent"), "error identifies the parent boundary: {error}");
    assert!(error.contains("missing"), "error retains the unresolved address: {error}");
}
