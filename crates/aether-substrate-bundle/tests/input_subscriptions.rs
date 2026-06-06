//! ADR-0021 publish/subscribe round-trip via [`TestBench`]. Loads
//! `aether-test-fixtures`'s `probe` cdylib into a real chassis and exercises
//! `aether.input.subscribe` / `aether.input.unsubscribe` and the
//! `aether.component.drop` lifecycle's effect on the input subscriber
//! set. Replaces the pre-issue-634-Phase-4 harness which drove the
//! retired `InputCapability::for_test` / `subscribe_for_test` /
//! `unsubscribe_for_test` helpers and used `mailer.drain_all` to
//! synchronise. Tracked under issue 648.

use std::path::Path;

use aether_actor::Actor;
use aether_capabilities::{ComponentHostCapability, InputCapability};
use aether_data::{Kind, KindId, MailboxId};
use aether_kinds::{
    DropComponent, DropResult, LoadComponent, LoadResult, SubscribeInputResult, Tick,
    UnsubscribeInput,
};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};
use aether_test_fixtures::TickObserved;
use std::fs;

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

/// No probes loaded ⇒ no Tick subscribers ⇒ advance generates zero
/// `tick_observed` broadcasts. Confirms the input fanout is gated on
/// the subscriber set rather than firing unconditionally.
#[test]
fn empty_subscribers_means_no_delivery() {
    if require_runtime("probe").is_none() {
        return;
    }
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    bench
        .execute(vec![("advance", BenchOp::advance(2))])
        .expect("advance 2");
    assert_eq!(
        bench.count_observed(TickObserved::NAME),
        0,
        "no probe loaded but tick_observed was broadcast; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// One subscribed probe broadcasts once per tick.
#[test]
fn subscribed_component_receives_published_ticks() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let _mbox = load_probe_named(&mut bench, &wasm_path, "listener");
    let baseline = bench.count_observed(TickObserved::NAME);

    bench
        .execute(vec![("advance", BenchOp::advance(3))])
        .expect("advance 3");
    let delta = bench.count_observed(TickObserved::NAME) - baseline;
    assert_eq!(
        delta,
        3,
        "expected 3 tick_observed broadcasts; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// Two independently-loaded probes each subscribe their own mailbox
/// in `wire`; tick fanout reaches both. 2 subscribers × 2 ticks ⇒
/// 4 broadcasts.
#[test]
fn two_subscribers_each_receive_every_tick() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let _mbox_a = load_probe_named(&mut bench, &wasm_path, "a");
    let _mbox_b = load_probe_named(&mut bench, &wasm_path, "b");
    let baseline = bench.count_observed(TickObserved::NAME);

    bench
        .execute(vec![("advance", BenchOp::advance(2))])
        .expect("advance 2");
    let delta = bench.count_observed(TickObserved::NAME) - baseline;
    assert_eq!(
        delta,
        4,
        "2 subscribers × 2 ticks should yield 4 broadcasts; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// `aether.input.unsubscribe` removes the mailbox from the Tick
/// subscriber set; subsequent advances stop producing broadcasts
/// from that probe.
#[test]
fn unsubscribe_stops_delivery() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let mbox = load_probe_named(&mut bench, &wasm_path, "listener");
    let baseline = bench.count_observed(TickObserved::NAME);

    bench
        .execute(vec![("advance", BenchOp::advance(1))])
        .expect("pre-unsubscribe advance");
    assert_eq!(
        bench.count_observed(TickObserved::NAME) - baseline,
        1,
        "expected 1 broadcast in the pre-unsubscribe window; observed kinds: {:?}",
        bench.observed_kinds(),
    );
    let pre_unsub = bench.count_observed(TickObserved::NAME);

    unsubscribe(&mut bench, Tick::ID, mbox);
    bench
        .execute(vec![("advance", BenchOp::advance(2))])
        .expect("post-unsubscribe advance");
    assert_eq!(
        bench.count_observed(TickObserved::NAME),
        pre_unsub,
        "tick_observed climbed after unsubscribe; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// `aether.component.drop` clears the dropped mailbox from the input
/// subscriber set as a side effect of lifecycle teardown
/// (ADR-0021 + ADR-0038). Subsequent advances don't broadcast from
/// the dropped probe.
#[test]
fn drop_clears_subscriptions() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let mbox = load_probe_named(&mut bench, &wasm_path, "victim");
    let baseline = bench.count_observed(TickObserved::NAME);

    bench
        .execute(vec![("advance", BenchOp::advance(1))])
        .expect("pre-drop advance");
    assert_eq!(
        bench.count_observed(TickObserved::NAME) - baseline,
        1,
        "expected 1 broadcast in the pre-drop window; observed kinds: {:?}",
        bench.observed_kinds(),
    );
    let pre_drop = bench.count_observed(TickObserved::NAME);

    drop_component(&mut bench, mbox);
    bench
        .execute(vec![("advance", BenchOp::advance(2))])
        .expect("post-drop advance");
    assert_eq!(
        bench.count_observed(TickObserved::NAME),
        pre_drop,
        "tick_observed climbed after drop; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}
