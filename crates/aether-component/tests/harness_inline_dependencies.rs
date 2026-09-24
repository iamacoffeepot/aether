//! Inline-spawnable dependency refusal (ADR-0230 §3, issue #6455).
//!
//! The inline-dependency fixture's default export `Holder` spawns the
//! composable `Needy` inline, and `Needy` declares
//! `depends(ClipboardCapability)`. The host checks that declaration when the
//! module loads, before `Holder` runs: a load on a harness without clipboard
//! is a `LoadResult::Err` naming `Needy`, and the same load is `Ok` once the
//! in-memory clipboard is composed. A replace toward the module is refused the
//! same way, and the replaced victim keeps serving.
//!
//! A private inline child (issue 6590) is checked the same way. The fs-demux
//! fixture's `InlineFsDemuxParent` declares no dependency, but its private
//! child `InlineFsDemuxChild` declares `depends(FsCapability, …)`, which the
//! host reads from the module's `aether.kinds.inputs.private` section: a load
//! of the parent on a harness without fs roots is refused naming the child.

use std::fs;

use aether_clipboard::{ClipboardCapability, ClipboardParams};
use aether_component::{ComponentHostCapability, WasmTrampoline};
use aether_data::{ActorPath, LoadName};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::Bump;

const HOLDER_EXPORT: &str = "test.inline_dependency.holder";
const TARGET_EXPORT: &str = "test.parent_peer.target";
const REFUSAL: &str = "test.inline_dependency.needy depends on aether.clipboard, which is not live";
const TICK_OBSERVED: &str = "aether.test_fixture.tick_observed";
const FS_DEMUX_PARENT_EXPORT: &str = "test.inline.fs_demux_parent";
const PRIVATE_REFUSAL: &str = "test.inline.fs_demux_child depends on aether.fs, which is not live";

fn load_result(harness: &mut SubstrateHarness, wasm: &[u8], label: &str, name: &str, export: &str) -> LoadResult {
    let component = LoadComponent {
        wasm: wasm.to_vec(),
        name: Some(name.to_owned()),
        config: Vec::new(),
        export: Some(export.to_owned()),
    };
    let operation = HarnessOp::send_and_await_reply(&harness.actor_ref::<ComponentHostCapability>(), &component);
    let result = harness.execute(vec![(label, operation)]).expect("component load operation");
    result.reply::<LoadResult>(label).expect("decode LoadResult")
}

fn key(name: &str) -> LoadName {
    LoadName::new(name).expect("a valid load name")
}

fn fixture_wasm() -> Option<Vec<u8>> {
    let wasm_path = require_wasm("aether_test_fixtures_inline_dependency")?;
    Some(fs::read(wasm_path).expect("read fixture wasm"))
}

#[test]
fn an_inline_child_dependency_refuses_its_module_load() {
    let Some(wasm) = fixture_wasm() else {
        return;
    };

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let LoadResult::Err { error } = load_result(&mut harness, &wasm, "absent", "holder", HOLDER_EXPORT) else {
        panic!("a module whose inline child's dependency is not live must be refused");
    };
    assert_eq!(error, REFUSAL, "the refusal names the inline child and the missing namespace");

    // Refusal happens before creation: no trampoline stands under the name.
    let host = harness.actor_ref::<ComponentHostCapability>();
    let unserved = harness.child::<ComponentHostCapability, WasmTrampoline>(&host, key("holder"));
    assert!(unserved.is_err(), "the refused load must not have created anything: {unserved:?}");

    let mut satisfied = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .with_actor::<ClipboardCapability>(ClipboardParams::InMemory)
        .build()
        .expect("boot with clipboard");
    match load_result(&mut satisfied, &wasm, "present", "holder", HOLDER_EXPORT) {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("a module whose inline child's dependency is live must load: {error}"),
    }
}

/// The satisfied path is `inline_child_matches_host_replies_to_its_own_requests`
/// in `inline_child.rs`, which loads the same export with fs roots composed.
#[test]
fn a_private_inline_child_dependency_refuses_its_module_load() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_fs_demux") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read fs-demux fixture wasm");

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let LoadResult::Err { error } = load_result(&mut harness, &wasm, "absent", "fs-demux", FS_DEMUX_PARENT_EXPORT)
    else {
        panic!("a module whose private inline child's dependency is not live must be refused");
    };
    assert_eq!(error, PRIVATE_REFUSAL, "the refusal names the private child and the missing namespace");

    // Refusal happens before creation: no trampoline stands under the name.
    let host = harness.actor_ref::<ComponentHostCapability>();
    let unserved = harness.child::<ComponentHostCapability, WasmTrampoline>(&host, key("fs-demux"));
    assert!(unserved.is_err(), "the refused load must not have created anything: {unserved:?}");
}

#[test]
fn a_replace_toward_an_unmet_inline_dependency_keeps_the_running_module() {
    let Some(wasm) = fixture_wasm() else {
        return;
    };
    let Some(bundle_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let bundle = fs::read(bundle_path).expect("read bundle wasm");

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let victim = match load_result(&mut harness, &bundle, "victim", "victim", TARGET_EXPORT) {
        LoadResult::Ok { path, .. } => path.to_string(),
        LoadResult::Err { error } => panic!("the victim must load: {error}"),
    };

    let operation = HarnessOp::send_and_await_reply(
        &harness.actor_ref::<ComponentHostCapability>(),
        &ReplaceComponent {
            target: ActorPath::new(&victim).expect("a loaded component's address is an actor path"),
            wasm,
            drain_timeout_ms: None,
            config: Vec::new(),
            export: Some(HOLDER_EXPORT.to_owned()),
        },
    );
    let result = harness.execute(vec![("replace", operation)]).expect("replace sequence");
    let ReplaceResult::Err { error } = result.reply::<ReplaceResult>("replace").expect("decode ReplaceResult") else {
        panic!("a replace toward a module whose inline child's dependency is not live must be refused");
    };
    assert_eq!(error, REFUSAL, "the refusal names the inline child and the missing namespace");

    // A refused replacement keeps the running module: the victim still
    // answers `Bump` with exactly one `TickObserved`.
    let host = harness.actor_ref::<ComponentHostCapability>();
    let victim_trampoline =
        harness.child::<ComponentHostCapability, WasmTrampoline>(&host, key("victim")).expect("the victim is live");
    let baseline = harness.count_observed(TICK_OBSERVED);
    harness
        .execute(vec![("bump", HarnessOp::send_and_settle(victim_trampoline.erase(), &Bump))])
        .expect("bump the victim");
    assert_eq!(
        harness.count_observed(TICK_OBSERVED),
        baseline + 1,
        "the victim must still serve after a refused replace"
    );
}
