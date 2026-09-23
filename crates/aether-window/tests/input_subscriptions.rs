//! ADR-0164 window-subscription round-trip via [`SubstrateHarness`]. Loads
//! `aether-test-fixtures`'s `probe` cdylib into a real chassis and exercises
//! selector-aware subscribe / unsubscribe plus the `aether.component.drop`
//! lifecycle's effect on the window subscriber set.
//!
//! Minimal composition (issue #3764): the component host (probe wasm) on
//! harness basics, which include the synthetic window runtime — no render,
//! no wgpu gate.
//! The probe wasm must be pre-built (`require_wasm` skips otherwise;
//! `AETHER_REQUIRE_RUNTIME=1` turns the skip into a hard failure).
//!
//! Targets the `Key` input stream, not `Tick`: issue 1490 moved `Tick`
//! onto `aether.lifecycle` because it is a frame-lifecycle stage, not a
//! window-originated interrupt. The probe subscribes `Key` for all windows
//! in `wire` and broadcasts a `key_observed` per dispatch; Tick-via-lifecycle
//! delivery is covered by the `substrate_harness` frame-loop scenarios.

use std::fs;
use std::path::Path;

use aether_actor::ErasedActorRef;
use aether_component::ComponentHostCapability;
use aether_data::{ActorPath, Kind};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, Key, LoadComponent, TextInput, WindowId};
use aether_test_fixtures_kinds::{KeyObserved, TextInputObserved, UnsubscribeKeys};
use aether_window::SyntheticWindowCapability;

/// Arbitrary key code for the synthetic `Key` events these tests inject.
const KEY_CODE: u32 = 65;
const TEST_WINDOW_ID: WindowId = WindowId(1);

fn boot_bench() -> SubstrateHarness {
    SubstrateHarness::builder().with_component_host().build().expect("boot")
}

fn load_probe_named(harness: &mut SubstrateHarness, wasm_path: &Path, name: &str) -> (ErasedActorRef, ActorPath) {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    harness
        .load_any(&LoadComponent { wasm, name: Some(name.to_owned()), config: Vec::new(), export: None })
        .unwrap_or_else(|error| panic!("load_component({name}): {error}"))
}

/// Inject `count` synthetic `Key` presses from one window. The synthetic
/// window actor fans each out to every matching subscriber; `execute` blocks on
/// settlement, so the `key_observed` broadcasts have landed by return.
fn send_keys(harness: &mut SubstrateHarness, count: usize) {
    let synthetic = harness.actor_ref::<SyntheticWindowCapability>();
    let labels: Vec<String> = (0..count).map(|i| format!("key{i}")).collect();
    let steps: Vec<(&str, HarnessOp)> = labels
        .iter()
        .map(|label| {
            let key = Key { window: TEST_WINDOW_ID, code: KEY_CODE };
            (label.as_str(), HarnessOp::window_event(&synthetic, TEST_WINDOW_ID, &key))
        })
        .collect();
    harness.execute(steps).expect("key send sequence");
}

/// Have `probe` unsubscribe itself from `Key` on every window.
fn unsubscribe_keys(harness: &mut SubstrateHarness, probe: ErasedActorRef) {
    harness
        .execute(vec![("unsub", HarnessOp::send_and_settle(probe, &UnsubscribeKeys))])
        .expect("unsubscribe sequence");
}

fn drop_component(harness: &mut SubstrateHarness, path: ActorPath) {
    let result = harness
        .execute(vec![(
            "drop",
            HarnessOp::send_and_await_reply(
                &harness.actor_ref::<ComponentHostCapability>(),
                &DropComponent { target: path },
            ),
        )])
        .expect("drop sequence");
    match result.reply::<DropResult>("drop").expect("decode DropResult") {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop failed: {error}"),
    }
}

/// No probes loaded ⇒ no `Key` subscribers ⇒ an injected key event
/// fans out to no one. Confirms window fanout is gated on the
/// subscriber set rather than firing unconditionally.
#[test]
fn empty_subscribers_means_no_delivery() {
    if require_wasm("aether_test_fixtures_bundle").is_none() {
        return;
    }
    let mut harness = boot_bench();
    send_keys(&mut harness, 2);
    assert_eq!(
        harness.count_observed(KeyObserved::NAME),
        0,
        "no probe loaded but key_observed was broadcast; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

/// A subscribed probe receives fanned-out `TextInput`. The plausible bug
/// this guards: a window-originated kind that is published but not routed
/// to matching subscribers would silently disappear before the guest handler.
/// Injecting synthetic `TextInput` and observing the probe's re-broadcast
/// proves the generic window-event fan-out is wired.
#[test]
fn subscribed_component_receives_published_text_input() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = boot_bench();
    let _probe = load_probe_named(&mut harness, &wasm_path, "typist");
    let baseline = harness.count_observed(TextInputObserved::NAME);

    harness
        .execute(vec![(
            "text",
            HarnessOp::window_event(
                &harness.actor_ref::<SyntheticWindowCapability>(),
                TEST_WINDOW_ID,
                &TextInput { window: TEST_WINDOW_ID, text: "hi".to_owned() },
            ),
        )])
        .expect("text send sequence");

    let delta = harness.count_observed(TextInputObserved::NAME) - baseline;
    assert_eq!(delta, 1, "expected 1 text_input_observed broadcast; observed kinds: {:?}", harness.observed_kinds());
}

/// One subscribed probe broadcasts once per injected key.
#[test]
fn subscribed_component_receives_published_keys() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = boot_bench();
    let _probe = load_probe_named(&mut harness, &wasm_path, "listener");
    let baseline = harness.count_observed(KeyObserved::NAME);

    send_keys(&mut harness, 3);
    let delta = harness.count_observed(KeyObserved::NAME) - baseline;
    assert_eq!(delta, 3, "expected 3 key_observed broadcasts; observed kinds: {:?}", harness.observed_kinds());
}

/// Two independently-loaded probes each subscribe their own mailbox
/// in `wire`; key fanout reaches both. 2 subscribers × 2 keys ⇒
/// 4 broadcasts.
#[test]
fn two_subscribers_each_receive_every_key() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = boot_bench();
    let _probe_a = load_probe_named(&mut harness, &wasm_path, "a");
    let _probe_b = load_probe_named(&mut harness, &wasm_path, "b");
    let baseline = harness.count_observed(KeyObserved::NAME);

    send_keys(&mut harness, 2);
    let delta = harness.count_observed(KeyObserved::NAME) - baseline;
    assert_eq!(
        delta,
        4,
        "2 subscribers × 2 keys should yield 4 broadcasts; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

/// The probe's own all-window unsubscribe removes it from the `Key`
/// subscriber set; subsequent key events stop producing broadcasts from
/// that probe.
#[test]
fn unsubscribe_stops_delivery() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = boot_bench();
    let (probe, _) = load_probe_named(&mut harness, &wasm_path, "listener");
    let baseline = harness.count_observed(KeyObserved::NAME);

    send_keys(&mut harness, 1);
    assert_eq!(
        harness.count_observed(KeyObserved::NAME) - baseline,
        1,
        "expected 1 broadcast in the pre-unsubscribe window; observed kinds: {:?}",
        harness.observed_kinds(),
    );
    let pre_unsub = harness.count_observed(KeyObserved::NAME);

    unsubscribe_keys(&mut harness, probe);
    send_keys(&mut harness, 2);
    assert_eq!(
        harness.count_observed(KeyObserved::NAME),
        pre_unsub,
        "key_observed climbed after unsubscribe; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

/// `aether.component.drop` clears the dropped mailbox from the window
/// subscriber set as a side effect of lifecycle teardown
/// (ADR-0164 + ADR-0038). Subsequent key events don't broadcast from the
/// dropped probe.
#[test]
fn drop_clears_subscriptions() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = boot_bench();
    let (_, probe) = load_probe_named(&mut harness, &wasm_path, "victim");
    let baseline = harness.count_observed(KeyObserved::NAME);

    send_keys(&mut harness, 1);
    assert_eq!(
        harness.count_observed(KeyObserved::NAME) - baseline,
        1,
        "expected 1 broadcast in the pre-drop window; observed kinds: {:?}",
        harness.observed_kinds(),
    );
    let pre_drop = harness.count_observed(KeyObserved::NAME);

    drop_component(&mut harness, probe);
    send_keys(&mut harness, 2);
    assert_eq!(
        harness.count_observed(KeyObserved::NAME),
        pre_drop,
        "key_observed climbed after drop; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}
