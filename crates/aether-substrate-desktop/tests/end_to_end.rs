// End-to-end wiring test. Uses an inline WAT guest so the test stays
// self-contained; `tests/*` that need the real `aether-hello-component`
// guest pull it in separately.
//
// The test is deliberately shaped like the real substrate flow:
//   1. Registry populated with one component mailbox + one sink.
//   2. Queue + scheduler constructed over the component.
//   3. A single mail enqueued to the component.
//   4. The component's `receive` calls the `aether::send_mail` host
//      function, targeting the sink.
//   5. We wait for the queue to drain and assert the sink handler ran.
// That round-trip exercises: host-fn linking, SubstrateCtx routing,
// the Component/Sink dispatch branch inside the worker, and the frame
// barrier's push/decrement/wait cycle.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use aether_substrate_desktop::{
    Component, HubOutbound, Mailer, Registry, Scheduler, SubstrateCtx, host_fns,
    mail::{Mail, MailboxId},
};
use wasmtime::{Engine, Linker, Module};

fn forwards_to_sink_wat(sink_id: MailboxId) -> String {
    // ADR-0029 made mailbox ids 64-bit name hashes, so the sink's id
    // isn't a small constant — splice the actual hashed value into
    // the WAT as an i64.const literal.
    format!(
        r#"
(module
  (import "aether" "send_mail_p32"
    (func $send_mail (param i64 i64 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "receive_p32")
    (param $kind i64) (param $ptr i32) (param $byte_len i32) (param $count i32) (param $sender i32)
    (result i32)
    i64.const {sink_id}
    i64.const 99
    i32.const 0
    i32.const 0
    local.get $count
    call $send_mail))
"#,
        sink_id = sink_id.0,
    )
}

#[test]
fn tick_roundtrip_component_to_sink() {
    let engine = Engine::default();

    let registry = Arc::new(Registry::new());
    let component_mbox = registry.register_component("hello");

    let counter = Arc::new(AtomicU32::new(0));
    let c2 = Arc::clone(&counter);
    let sink_mbox = registry.register_sink(
        "heartbeat",
        Arc::new(move |_kind_id, _kind, _origin, _sender, _bytes, count| {
            c2.fetch_add(count, Ordering::SeqCst);
        }),
    );
    let module = Module::new(&engine, forwards_to_sink_wat(sink_mbox)).expect("compile wat");
    assert_eq!(component_mbox, MailboxId::from_name("hello"));
    assert_eq!(sink_mbox, MailboxId::from_name("heartbeat"));
    let queue = Arc::new(Mailer::new());

    let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
    host_fns::register(&mut linker).expect("register host fns");

    let ctx = SubstrateCtx::new(
        component_mbox,
        Arc::clone(&registry),
        Arc::clone(&queue),
        HubOutbound::disconnected(),
        aether_substrate_desktop::new_subscribers(),
    );
    let component = Component::instantiate(&engine, &linker, &module, ctx).expect("instantiate");

    let scheduler = Scheduler::new(registry, Arc::clone(&queue), 2);
    scheduler.add_component(component_mbox, component);

    // Drive three "frames" — each frame, enqueue one tick mail and wait.
    for frame in 1..=3u32 {
        queue.push(Mail::new(component_mbox, 1, vec![], frame));
        queue.drain_all();
    }

    // Sink saw count=1 + count=2 + count=3 = 6.
    assert_eq!(counter.load(Ordering::SeqCst), 1 + 2 + 3);
}

/// Regression for issue 157: batched mail to a single recipient must
/// be delivered in push order even with multiple workers contending
/// the component's mutex. Before the per-mailbox strand fix, two
/// workers popping sequential mails from the FIFO queue could invert
/// deliver order because `Mutex::lock()` is not FIFO under contention;
/// the race showed up as `PlayMove { row, col }` values landing in
/// the wrong cells during the tic-tac-toe batch smoke.
///
/// The test pushes N mails carrying `count = 1..=N` and lets the
/// guest forward each one to a sink that appends the received `count`
/// to a `Vec`. With the fix the vector ends up `[1, 2, ..., N]`; with
/// the pre-fix scheduler it scrambled on most runs with workers=2 and
/// N large enough to keep both workers busy.
#[test]
fn batched_mail_preserves_fifo_per_mailbox() {
    const N: u32 = 200;

    let engine = Engine::default();
    let registry = Arc::new(Registry::new());
    let component_mbox = registry.register_component("fifo");

    let recorded: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::with_capacity(N as usize)));
    let recorded_clone = Arc::clone(&recorded);
    let sink_mbox = registry.register_sink(
        "observer",
        Arc::new(move |_kind_id, _kind, _origin, _sender, _bytes, count| {
            recorded_clone.lock().unwrap().push(count);
        }),
    );

    let module = Module::new(&engine, forwards_to_sink_wat(sink_mbox)).expect("compile wat");

    let queue = Arc::new(Mailer::new());
    let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
    host_fns::register(&mut linker).expect("register host fns");

    let ctx = SubstrateCtx::new(
        component_mbox,
        Arc::clone(&registry),
        Arc::clone(&queue),
        HubOutbound::disconnected(),
        aether_substrate_desktop::new_subscribers(),
    );
    let component = Component::instantiate(&engine, &linker, &module, ctx).expect("instantiate");

    // ADR-0038: dispatch parallelism is one thread per component now,
    // not a shared pool — the original strand-claim race the pre-ADR-0038
    // fix guarded against no longer exists, because the per-component
    // dispatcher has a single consumer on an mpsc inbox. The test is
    // kept as a regression guard on FIFO-per-mailbox under the new
    // shape: with N=200 mails routed through a single router thread
    // and forwarded via mpsc, order must still match the push order.
    let scheduler = Scheduler::new(registry, Arc::clone(&queue), 2);
    scheduler.add_component(component_mbox, component);

    for i in 1..=N {
        queue.push(Mail::new(component_mbox, 1, vec![], i));
    }
    queue.drain_all();

    let got = recorded.lock().unwrap().clone();
    assert_eq!(got.len(), N as usize, "sink saw the wrong number of mails");
    let expected: Vec<u32> = (1..=N).collect();
    assert_eq!(
        got, expected,
        "sink recorded out-of-order mail: per-mailbox FIFO broken"
    );
}
