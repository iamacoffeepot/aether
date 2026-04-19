// End-to-end wiring test for milestone 1 PR A. Uses an inline WAT guest
// to avoid pulling in a separate guest crate at this stage — PR B adds
// the real `aether-hello-component` guest and the build.rs integration.
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
use std::sync::atomic::{AtomicU32, Ordering};

use aether_substrate::{
    Component, HubOutbound, MailQueue, Registry, Scheduler, SubstrateCtx, host_fns,
    mail::{Mail, MailboxId},
};
use wasmtime::{Engine, Linker, Module};

const WAT: &str = r#"
(module
  (import "aether" "send_mail_p32"
    (func $send_mail (param i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "receive_p32")
    (param $kind i32) (param $ptr i32) (param $count i32) (param $sender i32)
    (result i32)
    ;; Forward a send_mail call to mailbox 1 (the sink in this test).
    ;; recipient=1, kind=99, ptr=0, len=0, count=<same as incoming count>.
    i32.const 1
    i32.const 99
    i32.const 0
    i32.const 0
    local.get $count
    call $send_mail))
"#;

#[test]
fn tick_roundtrip_component_to_sink() {
    let engine = Engine::default();
    let module = Module::new(&engine, WAT).expect("compile wat");

    let registry = Arc::new(Registry::new());
    let component_mbox = registry.register_component("hello");

    let counter = Arc::new(AtomicU32::new(0));
    let c2 = Arc::clone(&counter);
    let sink_mbox = registry.register_sink(
        "heartbeat",
        Arc::new(move |_kind, _origin, _sender, _bytes, count| {
            c2.fetch_add(count, Ordering::SeqCst);
        }),
    );
    assert_eq!(component_mbox, MailboxId(0));
    assert_eq!(sink_mbox, MailboxId(1));
    let queue = Arc::new(MailQueue::new());

    let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
    host_fns::register(&mut linker).expect("register host fns");

    let ctx = SubstrateCtx::new(
        component_mbox,
        Arc::clone(&registry),
        Arc::clone(&queue),
        HubOutbound::disconnected(),
    );
    let component = Component::instantiate(&engine, &linker, &module, ctx).expect("instantiate");

    let mut components = std::collections::HashMap::new();
    components.insert(component_mbox, component);
    let _scheduler = Scheduler::new(registry, Arc::clone(&queue), components, 2);

    // Drive three "frames" — each frame, enqueue one tick mail and wait.
    for frame in 1..=3u32 {
        queue.push(Mail::new(component_mbox, 1, vec![], frame));
        queue.wait_idle();
    }

    // Sink saw count=1 + count=2 + count=3 = 6.
    assert_eq!(counter.load(Ordering::SeqCst), 1 + 2 + 3);
}
