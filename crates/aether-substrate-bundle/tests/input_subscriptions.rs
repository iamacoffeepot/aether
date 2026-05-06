// End-to-end smoke for ADR-0021 publish/subscribe routing. Stands up
// the same triple a chassis main builds — Registry, Mailer, and
// `ControlPlaneCapability` sharing one `InputSubscribers` table — loads
// a WAT component via the cap's test-support entry, subscribes it,
// then drives "platform events" by calling the same `subscribers_for`
// helper that `App::window_event` uses and pushing one mail per
// subscriber. The WAT guest forwards every `receive` into a counting
// sink so the test observes end-to-end delivery without peeking
// into guest memory.
//
// Issue 603 retired the standalone `aether-substrate::control::ControlPlane`
// in favour of `aether-capabilities::ControlPlaneCapability`. The
// harness here uses the cap's `for_test` constructor to build the cap
// without spinning up a chassis dispatcher thread; the typed
// `*_for_test` methods drive the cap's internal handlers
// synchronously, which matches the pre-603 `dispatch(&plane, ...)`
// shape end-to-end minus the dispatcher hop.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use aether_capabilities::{ControlPlaneCapability, ControlPlaneConfig};
use aether_data::{Kind, KindId};
use aether_kinds::{DropComponent, LoadComponent, SubscribeInput, Tick, UnsubscribeInput};
use aether_substrate_bundle::{
    HubOutbound, InputSubscribers, Mailer, Registry, host_fns, mail::Mail, new_subscribers,
    subscribers_for,
};
use wasmtime::{Engine, Linker};

/// Minimal guest: forwards every `receive` to the `"tally"` sink the
/// harness registers, with a fixed kind id and a count of 1.
/// Byte-identical for Tick / Key / any other input kind — the test
/// doesn't care about the payload shape, only that the dispatch
/// arrived. The recipient id is spliced in at harness-build time
/// because ADR-0029 made mailbox ids 64-bit name hashes.
fn tally_forwarding_wat(tally_id: u64) -> String {
    format!(
        r#"
(module
  (import "aether" "send_mail_p32"
    (func $send_mail (param i64 i64 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "receive_p32")
    (param $kind i64) (param $ptr i32) (param $byte_len i32) (param $count i32) (param $sender i32)
    (result i32)
    (drop (call $send_mail
        (i64.const {tally_id}) (i64.const 99) (i32.const 0) (i32.const 0) (i32.const 1)))
    i32.const 0))
"#,
    )
}

struct Harness {
    cap: ControlPlaneCapability,
    queue: Arc<Mailer>,
    input_subscribers: InputSubscribers,
    counter: Arc<AtomicU32>,
    kind_tick: KindId,
    wat: String,
}

fn make_harness() -> Harness {
    let engine = Arc::new(Engine::default());
    let mut linker: Linker<aether_substrate_bundle::SubstrateCtx> = Linker::new(&engine);
    host_fns::register(&mut linker).expect("register host fns");
    let linker = Arc::new(linker);

    let registry = Arc::new(Registry::new());
    for d in aether_kinds::descriptors::all() {
        registry
            .register_kind_with_descriptor(d)
            .expect("descriptor unique");
    }
    let kind_tick = registry.kind_id(Tick::NAME).expect("Tick registered");

    let counter = Arc::new(AtomicU32::new(0));
    let c2 = Arc::clone(&counter);
    let sink_mbox = registry.register_sink(
        "tally",
        Arc::new(move |_kind_id, _kind, _origin, _sender, _bytes, count| {
            c2.fetch_add(count, Ordering::SeqCst);
        }),
    );
    let wat = tally_forwarding_wat(sink_mbox.0);

    let queue = Arc::new(Mailer::new());
    queue.wire(Arc::clone(&registry));

    let input_subscribers = new_subscribers();
    let cap = ControlPlaneCapability::for_test(
        ControlPlaneConfig {
            engine,
            linker,
            hub_outbound: HubOutbound::disconnected(),
            input_subscribers: Arc::clone(&input_subscribers),
            chassis_handler: None,
        },
        Arc::clone(&registry),
        Arc::clone(&queue),
    );

    Harness {
        cap,
        queue,
        input_subscribers,
        counter,
        kind_tick,
        wat,
    }
}

fn load_wat(cap: &ControlPlaneCapability, wat: &str, name: &str) -> aether_data::MailboxId {
    let payload = LoadComponent {
        wasm: wat::parse_str(wat).expect("compile WAT"),
        name: Some(name.into()),
    };
    let result = cap.load_for_test(payload);
    match result {
        aether_kinds::LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        aether_kinds::LoadResult::Err { error } => panic!("load failed: {error}"),
    }
}

fn subscribe(cap: &ControlPlaneCapability, kind: KindId, mailbox: aether_data::MailboxId) {
    let r = cap.subscribe_for_test(SubscribeInput { kind, mailbox });
    matches!(r, aether_kinds::SubscribeInputResult::Ok)
        .then_some(())
        .expect("subscribe succeeded");
}

fn unsubscribe(cap: &ControlPlaneCapability, kind: KindId, mailbox: aether_data::MailboxId) {
    let r = cap.unsubscribe_for_test(UnsubscribeInput { kind, mailbox });
    matches!(r, aether_kinds::SubscribeInputResult::Ok)
        .then_some(())
        .expect("unsubscribe succeeded");
}

fn drop_component(cap: &ControlPlaneCapability, mailbox_id: aether_data::MailboxId) {
    let r = cap.drop_for_test(DropComponent { mailbox_id });
    matches!(r, aether_kinds::DropResult::Ok)
        .then_some(())
        .expect("drop succeeded");
}

/// Publish one Tick exactly as `App::window_event` does: snapshot the
/// subscriber set, push one mail per subscriber, block until the
/// dispatcher drains.
fn publish_tick(h: &Harness) {
    for mbox in subscribers_for(&h.input_subscribers, Tick::ID) {
        h.queue.push(Mail::new(mbox, h.kind_tick, vec![], 1));
    }
    h.queue.drain_all();
}

#[test]
fn empty_subscribers_means_no_delivery() {
    let h = make_harness();
    publish_tick(&h);
    publish_tick(&h);
    assert_eq!(h.counter.load(Ordering::SeqCst), 0);
    assert!(subscribers_for(&h.input_subscribers, Tick::ID).is_empty());
}

#[test]
fn subscribed_component_receives_published_ticks() {
    let h = make_harness();
    let id = load_wat(&h.cap, &h.wat, "listener");
    subscribe(&h.cap, Tick::ID, id);
    assert_eq!(subscribers_for(&h.input_subscribers, Tick::ID), vec![id]);
    for _ in 0..3 {
        publish_tick(&h);
    }
    assert_eq!(h.counter.load(Ordering::SeqCst), 3);
}

#[test]
fn two_subscribers_each_receive_every_tick() {
    let h = make_harness();
    let a = load_wat(&h.cap, &h.wat, "a");
    let b = load_wat(&h.cap, &h.wat, "b");
    subscribe(&h.cap, Tick::ID, a);
    subscribe(&h.cap, Tick::ID, b);
    publish_tick(&h);
    publish_tick(&h);
    // 2 subscribers × 2 ticks = 4 deliveries.
    assert_eq!(h.counter.load(Ordering::SeqCst), 4);
}

#[test]
fn unsubscribe_stops_delivery() {
    let h = make_harness();
    let id = load_wat(&h.cap, &h.wat, "listener");
    subscribe(&h.cap, Tick::ID, id);
    publish_tick(&h);
    assert_eq!(h.counter.load(Ordering::SeqCst), 1);

    unsubscribe(&h.cap, Tick::ID, id);
    publish_tick(&h);
    publish_tick(&h);
    assert_eq!(h.counter.load(Ordering::SeqCst), 1);
}

#[test]
fn drop_clears_subscriptions() {
    let h = make_harness();
    let id = load_wat(&h.cap, &h.wat, "victim");
    subscribe(&h.cap, Tick::ID, id);
    publish_tick(&h);
    assert_eq!(h.counter.load(Ordering::SeqCst), 1);

    drop_component(&h.cap, id);
    assert!(subscribers_for(&h.input_subscribers, Tick::ID).is_empty());
    publish_tick(&h);
    publish_tick(&h);
    assert_eq!(h.counter.load(Ordering::SeqCst), 1);
}
