//! ADR-0021 publish/subscribe round-trip via [`TestBench`]. Loads
//! `aether-test-fixtures`'s `probe` cdylib into a real chassis and exercises
//! `aether.input.subscribe` / `aether.input.unsubscribe` and the
//! `aether.component.drop` lifecycle's effect on the input subscriber
//! set. Replaces the pre-issue-634-Phase-4 harness which drove the
//! retired `InputCapability::for_test` / `subscribe_for_test` /
//! `unsubscribe_for_test` helpers and used `mailer.drain_all` to
//! synchronise. Tracked under issue 648.
//!
//! Targets the `Key` input stream, not `Tick`: issue 1490 moved `Tick`
//! off `aether.input` onto `aether.lifecycle` (it is a frame-lifecycle
//! stage, not an input interrupt), so the `aether.input` subscribe /
//! unsubscribe / drop-clears contract is exercised here against a
//! genuine input stream. The probe subscribes `Key` in `wire` and
//! broadcasts a `key_observed` per dispatch; Tick-via-lifecycle delivery
//! is covered by the `test_bench` frame-loop scenarios.

use std::path::Path;

use aether_actor::Actor;
use aether_capabilities::{ComponentHostCapability, InputCapability};
use aether_data::{Kind, KindId, MailboxId};
use aether_kinds::{
    DropComponent, DropResult, Key, LoadComponent, LoadResult, SubscribeInputResult,
    UnsubscribeInput,
};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};
use aether_test_fixtures::KeyObserved;
use std::fs;

/// Arbitrary key code for the synthetic `Key` events these tests inject.
const KEY_CODE: u32 = 65;

fn load_probe_named(bench: &mut TestBench, wasm_path: &Path, name: &str) -> MailboxId {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: Some(name.to_owned()),
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("load sequence");
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("load_component({name}): {error}"),
    }
}

/// Inject `count` synthetic `Key` presses to `aether.input`. The input
/// cap fans each out to every `Key` subscriber; `execute` blocks on
/// settlement, so the `key_observed` broadcasts have landed by return.
fn send_keys(bench: &mut TestBench, count: usize) {
    let labels: Vec<String> = (0..count).map(|i| format!("key{i}")).collect();
    let steps: Vec<(&str, BenchOp)> = labels
        .iter()
        .map(|label| {
            (
                label.as_str(),
                BenchOp::send_mail("aether.input", &Key { code: KEY_CODE }),
            )
        })
        .collect();
    bench.execute(steps).expect("key send sequence");
}

fn unsubscribe(bench: &mut TestBench, kind: KindId, mailbox: MailboxId) {
    let result = bench
        .execute(vec![(
            "unsub",
            BenchOp::send_and_await(
                InputCapability::NAMESPACE,
                &UnsubscribeInput { kind, mailbox },
            ),
        )])
        .expect("unsubscribe sequence");
    match result
        .reply::<SubscribeInputResult>("unsub")
        .expect("decode SubscribeInputResult")
    {
        SubscribeInputResult::Ok => {}
        SubscribeInputResult::Err { error } => panic!("unsubscribe failed: {error}"),
    }
}

fn drop_component(bench: &mut TestBench, mailbox_id: MailboxId) {
    let result = bench
        .execute(vec![(
            "drop",
            BenchOp::send_and_await(
                ComponentHostCapability::NAMESPACE,
                &DropComponent { mailbox_id },
            ),
        )])
        .expect("drop sequence");
    match result
        .reply::<DropResult>("drop")
        .expect("decode DropResult")
    {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop failed: {error}"),
    }
}

/// No probes loaded ⇒ no `Key` subscribers ⇒ an injected key event
/// fans out to no one. Confirms the input fanout is gated on the
/// subscriber set rather than firing unconditionally.
#[test]
fn empty_subscribers_means_no_delivery() {
    if require_runtime("probe").is_none() {
        return;
    }
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    send_keys(&mut bench, 2);
    assert_eq!(
        bench.count_observed(KeyObserved::NAME),
        0,
        "no probe loaded but key_observed was broadcast; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// One subscribed probe broadcasts once per injected key.
#[test]
fn subscribed_component_receives_published_keys() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let _mbox = load_probe_named(&mut bench, &wasm_path, "listener");
    let baseline = bench.count_observed(KeyObserved::NAME);

    send_keys(&mut bench, 3);
    let delta = bench.count_observed(KeyObserved::NAME) - baseline;
    assert_eq!(
        delta,
        3,
        "expected 3 key_observed broadcasts; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// Two independently-loaded probes each subscribe their own mailbox
/// in `wire`; key fanout reaches both. 2 subscribers × 2 keys ⇒
/// 4 broadcasts.
#[test]
fn two_subscribers_each_receive_every_key() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let _mbox_a = load_probe_named(&mut bench, &wasm_path, "a");
    let _mbox_b = load_probe_named(&mut bench, &wasm_path, "b");
    let baseline = bench.count_observed(KeyObserved::NAME);

    send_keys(&mut bench, 2);
    let delta = bench.count_observed(KeyObserved::NAME) - baseline;
    assert_eq!(
        delta,
        4,
        "2 subscribers × 2 keys should yield 4 broadcasts; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// `aether.input.unsubscribe` removes the mailbox from the `Key`
/// subscriber set; subsequent key events stop producing broadcasts
/// from that probe.
#[test]
fn unsubscribe_stops_delivery() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let mbox = load_probe_named(&mut bench, &wasm_path, "listener");
    let baseline = bench.count_observed(KeyObserved::NAME);

    send_keys(&mut bench, 1);
    assert_eq!(
        bench.count_observed(KeyObserved::NAME) - baseline,
        1,
        "expected 1 broadcast in the pre-unsubscribe window; observed kinds: {:?}",
        bench.observed_kinds(),
    );
    let pre_unsub = bench.count_observed(KeyObserved::NAME);

    unsubscribe(&mut bench, Key::ID, mbox);
    send_keys(&mut bench, 2);
    assert_eq!(
        bench.count_observed(KeyObserved::NAME),
        pre_unsub,
        "key_observed climbed after unsubscribe; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// `aether.component.drop` clears the dropped mailbox from the input
/// subscriber set as a side effect of lifecycle teardown
/// (ADR-0021 + ADR-0038). Subsequent key events don't broadcast from
/// the dropped probe.
#[test]
fn drop_clears_subscriptions() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let mbox = load_probe_named(&mut bench, &wasm_path, "victim");
    let baseline = bench.count_observed(KeyObserved::NAME);

    send_keys(&mut bench, 1);
    assert_eq!(
        bench.count_observed(KeyObserved::NAME) - baseline,
        1,
        "expected 1 broadcast in the pre-drop window; observed kinds: {:?}",
        bench.observed_kinds(),
    );
    let pre_drop = bench.count_observed(KeyObserved::NAME);

    drop_component(&mut bench, mbox);
    send_keys(&mut bench, 2);
    assert_eq!(
        bench.count_observed(KeyObserved::NAME),
        pre_drop,
        "key_observed climbed after drop; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}
