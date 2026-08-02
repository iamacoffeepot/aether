//! Post-issue-634-Phase-4 end-to-end smoke for the trampoline-as-actor
//! routing path. Boots a [`SubstrateHarness`], loads `aether-test-fixtures`'s `probe`
//! into it via the same `aether.component` mail surface a hub-driven
//! session uses, and asserts the wasm host-fn call chain
//! (`ctx.send_to_named(SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME, &TickObserved)`)
//! reaches the harness's loopback observation queue. Issue 775 retired
//! the previous `ctx.actor::<BroadcastCapability>().send(...)` shape;
//! the substrate-harness observer mailbox replaced the broadcast cap for
//! scenario observation.
//!
//! Replaces the pre-Phase-4 WAT-driven harness which drove a hand-built
//! `Component` past the cap's dispatcher infrastructure via the retired
//! `ComponentHostCapability::for_test` / `attach_component_for_test`
//! helpers. Phase 4 moved that dispatch onto the framework's
//! `NativeActor` trampoline, so the equivalent coverage runs through a
//! real load + advance against a wasm component. Tracked under issue 648.

use std::path::Path;

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_data::{Kind, MailboxId};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_test_fixtures_kinds::TickObserved;
use std::fs;

const PROBE_NAME: &str = "probe";

fn load_probe(harness: &mut SubstrateHarness, wasm_path: &Path) -> MailboxId {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: Some(PROBE_NAME.to_owned()),
                    config: Vec::new(),
                    export: None,
                    replica: None,
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

/// Tick fanout reaches a freshly-loaded wasm component, the
/// component's `ctx.send_to_named(SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME, &...)`
/// host call lands the kind on the harness's loopback observation
/// queue, and `count_observed` sees it. End-to-end proof of host-fn
/// linking, trampoline dispatch, `wire`-time input subscription, and
/// outbound mail routing.
#[test]
fn tick_roundtrip_component_to_sink() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let _mbox = load_probe(&mut harness, &wasm_path);
    let baseline = harness.count_observed(TickObserved::NAME);

    harness.execute(vec![("advance", HarnessOp::advance(3))]).expect("advance 3");
    let delta = harness.count_observed(TickObserved::NAME) - baseline;
    assert_eq!(
        delta,
        3,
        "expected 3 tick_observed broadcasts after advance(3); observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

/// No-drops guard for the trampoline dispatch loop under volume.
/// Pre-ADR-0038 a worker-pool race could invert per-mailbox order
/// under contention; ADR-0038 collapsed dispatch to one mpsc consumer
/// per actor so the original strand-claim race is structurally
/// impossible. The "no drops at N" property still matters as a smoke
/// for the trampoline pump — pushes 200 ticks through one component
/// and asserts every broadcast made the round trip back.
#[test]
fn batched_ticks_preserve_per_mailbox_count() {
    const N: u32 = 200;

    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let _mbox = load_probe(&mut harness, &wasm_path);
    let baseline = harness.count_observed(TickObserved::NAME);

    harness.execute(vec![("advance", HarnessOp::advance(N))]).expect("advance N");
    let delta = harness.count_observed(TickObserved::NAME) - baseline;
    assert_eq!(
        delta,
        N as usize,
        "expected {N} tick_observed broadcasts after advance({N}); observed kinds: {:?}",
        harness.observed_kinds(),
    );
}
