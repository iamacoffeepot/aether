// End-to-end smoke for ADR-0021 publish/subscribe routing. The test
// stands up the same triple the substrate binary uses — Registry,
// Scheduler, ControlPlane sharing one `InputSubscribers` table —
// loads a WAT component via the control-plane sink handler,
// subscribes it, then drives "platform events" by calling the same
// `subscribers_for` helper that `App::window_event` uses and pushing
// one mail per subscriber. The WAT guest forwards every `receive`
// into a counting sink so the test can observe end-to-end delivery
// without peeking guest memory.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use aether_hub_protocol::SessionToken;
use aether_kinds::{
    DropComponent, InputStream, LoadComponent, SubscribeInput, Tick, UnsubscribeInput,
};
use aether_mail::Kind;
use aether_substrate::{
    AETHER_CONTROL, ControlPlane, HubOutbound, InputSubscribers, MailQueue, Registry, Scheduler,
    SubstrateCtx, host_fns,
    mail::{Mail, MailboxId},
    new_subscribers, subscribers_for,
};
use wasmtime::{Engine, Linker};

/// Minimal guest: forwards every `receive` to mailbox `1` (the
/// counting sink the harness registers) with a fixed kind id and a
/// count of 1. Byte-identical for Tick / Key / any other input kind
/// — the test doesn't care about the payload shape, only that the
/// dispatch arrived.
const WAT: &str = r#"
(module
  (import "aether" "send_mail_p32"
    (func $send_mail (param i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "receive_p32")
    (param $kind i32) (param $ptr i32) (param $count i32) (param $sender i32)
    (result i32)
    (drop (call $send_mail
        (i32.const 1) (i32.const 99) (i32.const 0) (i32.const 0) (i32.const 1)))
    i32.const 0))
"#;

struct Harness {
    plane: ControlPlane,
    queue: Arc<MailQueue>,
    input_subscribers: InputSubscribers,
    counter: Arc<AtomicU32>,
    kind_tick: u32,
    _scheduler: Scheduler,
}

fn make_harness() -> Harness {
    let engine = Arc::new(Engine::default());
    let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
    host_fns::register(&mut linker).expect("register host fns");
    let linker = Arc::new(linker);

    let registry = Arc::new(Registry::new());
    for d in aether_kinds::descriptors::all() {
        registry
            .register_kind_with_descriptor(d)
            .expect("descriptor unique");
    }
    let kind_tick = registry.kind_id(Tick::NAME).expect("Tick registered");

    // Mailbox id 0 is the counting sink — the WAT hard-codes `1` as
    // the forwarding target, so we register the sink first at id 0,
    // then correct the expectation below. Actually `register_sink`
    // allocates ids in order; the first registration gets id 0, so
    // we need the tally sink to land at id 1. Reserve id 0 with a
    // placeholder control sink first.
    let placeholder = registry.register_sink(
        AETHER_CONTROL,
        Arc::new(|_, _, _, _, _| { /* replaced below once plane exists */ }),
    );
    assert_eq!(placeholder.0, 0);

    let counter = Arc::new(AtomicU32::new(0));
    let c2 = Arc::clone(&counter);
    let sink_mbox = registry.register_sink(
        "tally",
        Arc::new(move |_kind, _origin, _sender, _bytes, count| {
            c2.fetch_add(count, Ordering::SeqCst);
        }),
    );
    assert_eq!(sink_mbox.0, 1, "WAT hardcodes mailbox 1 as the sink");

    let queue = Arc::new(MailQueue::new());
    let scheduler = Scheduler::new(
        Arc::clone(&registry),
        Arc::clone(&queue),
        std::collections::HashMap::new(),
        2,
    );

    let input_subscribers = new_subscribers();
    let plane = ControlPlane {
        engine: Arc::clone(&engine),
        linker: Arc::clone(&linker),
        registry: Arc::clone(&registry),
        queue: Arc::clone(&queue),
        outbound: HubOutbound::disconnected(),
        components: scheduler.components().clone(),
        input_subscribers: Arc::clone(&input_subscribers),
        default_name_counter: Arc::new(AtomicU64::new(0)),
        capture_queue: aether_substrate::CaptureQueue::new(),
        platform_info_notifier: Arc::new(aether_substrate::NoopPlatformInfoNotifier),
        window_mode_notifier: Arc::new(aether_substrate::NoopWindowModeNotifier),
    };

    Harness {
        plane,
        queue,
        input_subscribers,
        counter,
        kind_tick,
        _scheduler: scheduler,
    }
}

/// Dispatch a single control-plane kind by going through the public
/// sink-handler surface. `ControlPlane::into_sink_handler` consumes
/// its receiver, so the harness clones the plane (`Arc`-cheap) for
/// each call. Mirrors how main.rs wires the handler at boot.
fn dispatch<K: serde::Serialize>(plane: &ControlPlane, kind_name: &str, payload: &K) {
    let bytes = postcard::to_allocvec(payload).unwrap();
    let handler = plane.clone().into_sink_handler();
    handler(kind_name, None, SessionToken::NIL, &bytes, 0);
}

fn load_wat(plane: &ControlPlane, name: &str) -> u32 {
    let before: std::collections::HashSet<u32> = plane
        .components
        .read()
        .unwrap()
        .keys()
        .map(|m| m.0)
        .collect();
    dispatch(
        plane,
        LoadComponent::NAME,
        &LoadComponent {
            wasm: wat::parse_str(WAT).expect("compile WAT"),
            kinds: vec![],
            name: Some(name.into()),
        },
    );
    let after: std::collections::HashSet<u32> = plane
        .components
        .read()
        .unwrap()
        .keys()
        .map(|m| m.0)
        .collect();
    *after
        .difference(&before)
        .next()
        .expect("load inserted a new component")
}

fn subscribe(plane: &ControlPlane, stream: InputStream, mailbox: u32) {
    dispatch(
        plane,
        SubscribeInput::NAME,
        &SubscribeInput { stream, mailbox },
    );
}

fn unsubscribe(plane: &ControlPlane, stream: InputStream, mailbox: u32) {
    dispatch(
        plane,
        UnsubscribeInput::NAME,
        &UnsubscribeInput { stream, mailbox },
    );
}

fn drop_component(plane: &ControlPlane, mailbox_id: u32) {
    dispatch(plane, DropComponent::NAME, &DropComponent { mailbox_id });
}

/// Publish one Tick exactly as `App::window_event` does: snapshot the
/// subscriber set, push one mail per subscriber, block until the
/// scheduler drains.
fn publish_tick(h: &Harness) {
    for mbox in subscribers_for(&h.input_subscribers, InputStream::Tick) {
        h.queue.push(Mail::new(mbox, h.kind_tick, vec![], 1));
    }
    h.queue.wait_idle();
}

#[test]
fn empty_subscribers_means_no_delivery() {
    let h = make_harness();
    publish_tick(&h);
    publish_tick(&h);
    assert_eq!(h.counter.load(Ordering::SeqCst), 0);
    assert!(subscribers_for(&h.input_subscribers, InputStream::Tick).is_empty());
}

#[test]
fn subscribed_component_receives_published_ticks() {
    let h = make_harness();
    let id = load_wat(&h.plane, "listener");
    subscribe(&h.plane, InputStream::Tick, id);
    assert_eq!(
        subscribers_for(&h.input_subscribers, InputStream::Tick),
        vec![MailboxId(id)]
    );
    for _ in 0..3 {
        publish_tick(&h);
    }
    assert_eq!(h.counter.load(Ordering::SeqCst), 3);
}

#[test]
fn two_subscribers_each_receive_every_tick() {
    let h = make_harness();
    let a = load_wat(&h.plane, "a");
    let b = load_wat(&h.plane, "b");
    subscribe(&h.plane, InputStream::Tick, a);
    subscribe(&h.plane, InputStream::Tick, b);
    publish_tick(&h);
    publish_tick(&h);
    // 2 subscribers × 2 ticks = 4 deliveries.
    assert_eq!(h.counter.load(Ordering::SeqCst), 4);
}

#[test]
fn unsubscribe_stops_delivery() {
    let h = make_harness();
    let id = load_wat(&h.plane, "listener");
    subscribe(&h.plane, InputStream::Tick, id);
    publish_tick(&h);
    assert_eq!(h.counter.load(Ordering::SeqCst), 1);

    unsubscribe(&h.plane, InputStream::Tick, id);
    publish_tick(&h);
    publish_tick(&h);
    assert_eq!(h.counter.load(Ordering::SeqCst), 1);
}

#[test]
fn drop_clears_subscriptions() {
    let h = make_harness();
    let id = load_wat(&h.plane, "victim");
    subscribe(&h.plane, InputStream::Tick, id);
    publish_tick(&h);
    assert_eq!(h.counter.load(Ordering::SeqCst), 1);

    drop_component(&h.plane, id);
    assert!(subscribers_for(&h.input_subscribers, InputStream::Tick).is_empty());
    publish_tick(&h);
    publish_tick(&h);
    assert_eq!(h.counter.load(Ordering::SeqCst), 1);
}
