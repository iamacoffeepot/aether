//! Post-issue-634-Phase-4 end-to-end smoke for the trampoline-as-actor
//! routing path. Boots a [`TestBench`], loads `aether-test-fixture-probe`
//! into it via the same `aether.component` mail surface a hub-driven
//! session uses, and asserts the wasm host-fn call chain
//! (`ctx.actor::<BroadcastCapability>().send(&TickObserved)`) reaches
//! the bench's loopback observation queue.
//!
//! Replaces the pre-Phase-4 WAT-driven harness which drove a hand-built
//! `Component` past the cap's dispatcher infrastructure via the retired
//! `ComponentHostCapability::for_test` / `attach_component_for_test`
//! helpers. Phase 4 moved that dispatch onto the framework's
//! `NativeActor` trampoline, so the equivalent coverage runs through a
//! real load + advance against a wasm component. Tracked under issue 648.

use std::path::Path;

use aether_actor::Actor;
use aether_capabilities::{ComponentHostCapability, InputCapability};
use aether_data::{Kind, MailboxId};
use aether_kinds::{LoadComponent, LoadResult, SubscribeInput, SubscribeInputResult, Tick};
use aether_scenario::test_helpers::require_runtime;
use aether_substrate_bundle::test_bench::TestBench;
use aether_test_fixture_probe::TickObserved;

const PROBE_NAME: &str = "probe";

fn load_probe(bench: &mut TestBench, wasm_path: &Path) -> MailboxId {
    let wasm = std::fs::read(wasm_path).expect("read fixture wasm");
    let result: LoadResult = bench
        .send_and_await_reply(
            ComponentHostCapability::NAMESPACE,
            &LoadComponent {
                wasm,
                name: Some(PROBE_NAME.to_owned()),
            },
        )
        .expect("await load_component reply");
    match result {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

/// The probe self-subscribes to `Tick` from its `wire` handler via
/// fire-and-forget mail; `LoadResult` returns before that mail has
/// necessarily been drained by `InputCapability`'s actor. Mailing a
/// redundant `SubscribeInput` from the test side and awaiting its
/// reply gives a deterministic happens-before: `InputCapability` is
/// single-threaded mpsc-FIFO, so once our reply arrives the probe's
/// prior wire-time subscribe has been processed too. The set is a
/// `BTreeSet` so the duplicate insert is a no-op.
fn await_tick_subscribed(bench: &mut TestBench, mailbox: MailboxId) {
    let r: SubscribeInputResult = bench
        .send_and_await_reply(
            InputCapability::NAMESPACE,
            &SubscribeInput {
                kind: Tick::ID,
                mailbox,
            },
        )
        .expect("await redundant subscribe reply");
    match r {
        SubscribeInputResult::Ok => {}
        SubscribeInputResult::Err { error } => panic!("redundant subscribe failed: {error}"),
    }
}

/// Run one no-tick `capture()` after a measurement window so the
/// last advanced frame's trampoline broadcast (which can still be
/// in `BroadcastCapability`'s inbox if `wait_instanced_quiesce`
/// gave up early under heavy parallel-test CPU pressure) lands in
/// `observed_kinds` before we read the count. Capture runs a full
/// frame with `dispatch_tick = false`, so no new `tick_observed`
/// is produced — the count delta stays at exactly N.
fn settle_observations(bench: &mut TestBench) {
    let _png = bench.capture().expect("settle capture");
}

/// Tick fanout reaches a freshly-loaded wasm component, the
/// component's `ctx.actor::<BroadcastCapability>().send(...)` host
/// call lands a kind-tagged broadcast on the bench's loopback, and
/// `count_observed` sees it. End-to-end proof of host-fn linking,
/// trampoline dispatch, `wire`-time input subscription, and outbound
/// broadcast routing.
#[test]
fn tick_roundtrip_component_to_sink() {
    let Some(wasm_path) = require_runtime("aether_test_fixture_probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let mbox = load_probe(&mut bench, &wasm_path);
    await_tick_subscribed(&mut bench, mbox);
    let baseline = bench.count_observed(TickObserved::NAME);

    bench.advance(3).expect("advance 3");
    settle_observations(&mut bench);
    let delta = bench.count_observed(TickObserved::NAME) - baseline;
    assert_eq!(
        delta,
        3,
        "expected 3 tick_observed broadcasts after advance(3); observed kinds: {:?}",
        bench.observed_kinds(),
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

    let Some(wasm_path) = require_runtime("aether_test_fixture_probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let mbox = load_probe(&mut bench, &wasm_path);
    await_tick_subscribed(&mut bench, mbox);
    let baseline = bench.count_observed(TickObserved::NAME);

    bench.advance(N).expect("advance N");
    settle_observations(&mut bench);
    let delta = bench.count_observed(TickObserved::NAME) - baseline;
    assert_eq!(
        delta,
        N as usize,
        "expected {N} tick_observed broadcasts after advance({N}); observed kinds: {:?}",
        bench.observed_kinds(),
    );
}
