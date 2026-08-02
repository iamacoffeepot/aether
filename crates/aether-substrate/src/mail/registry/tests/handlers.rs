//! Tests for [`super::super::handlers`] — the `InboxHandler` and
//! `InlineHandler` blanket impls and the hand-rolled shape beside them.

use std::panic;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use aether_kinds::trace::Nanos;

use crate::mail::registry::{
    InboxHandler, InlineHandler, MailDispatch, MailboxEntry, OwnedDispatch, Registry, test_dispatch,
    test_owned_dispatch,
};
use crate::mail::{KindId, MailId, MailRef, MailboxId, Source};
use crate::testing::boot_authority as auth;

#[test]
fn closure_handler_runs_on_call() {
    let r = Registry::new();
    let counter = Arc::new(AtomicU32::new(0));
    let c2 = Arc::clone(&counter);
    let id = r.register_inbox(
        &auth(),
        "heartbeat",
        Arc::new(move |dispatch: OwnedDispatch| {
            c2.fetch_add(dispatch.count, Ordering::SeqCst);
        }),
    );
    let Some(MailboxEntry::Inbox { handler: h, .. }) = r.entry(id) else {
        panic!("expected closure entry")
    };
    // Test-side id is irrelevant — the handler ignores it.
    h.enqueue(test_owned_dispatch(KindId(0), &[], 7));
    h.enqueue(OwnedDispatch::disarmed(
        KindId(0),
        Some("physics".to_owned()),
        Source::NONE,
        MailRef::from(Vec::new()),
        3,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));
    assert_eq!(counter.load(Ordering::SeqCst), 10);
}

/// Issue iamacoffeepot/aether#848 Phase 1: a bare
/// `Fn(MailDispatch<'_>)` closure satisfies `InlineHandler` via
/// the blanket impl, and dispatching through
/// `<dyn InlineHandler>::dispatch` invokes the body once per
/// call. No mailer / registry plumbing is wired through yet —
/// that lands in PR 2.
#[test]
fn inline_handler_blanket_impl_dispatches_closure_body() {
    let counter = Arc::new(AtomicU32::new(0));
    let c2 = Arc::clone(&counter);
    let handler: Arc<dyn InlineHandler> = Arc::new(move |dispatch: MailDispatch<'_>| {
        c2.fetch_add(dispatch.count, Ordering::SeqCst);
    });
    handler.dispatch(test_dispatch(KindId(0), &[], 5));
    handler.dispatch(test_dispatch(KindId(0), &[], 7));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        12,
        "blanket InlineHandler impl should forward each dispatch to the closure body once",
    );
}

/// Issue iamacoffeepot/aether#848 Phase 1: a bare
/// `Fn(OwnedDispatch)` closure satisfies `InboxHandler` via the
/// blanket impl. The closure body moves the payload into a
/// captured Vec, demonstrating the ownership transfer the trait
/// exists to enable — the hot-path "no `to_vec()` clone" win
/// called out in iamacoffeepot/aether#848.
#[test]
fn inbox_handler_blanket_impl_moves_owned_payload() {
    let collected = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let collected_for_handler = Arc::clone(&collected);
    let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
        // Payload moves straight into the captured Vec — no clone
        // or `to_vec()` on a borrowed slice.
        collected_for_handler.lock().unwrap().push(dispatch.payload.into_vec());
    });

    handler.enqueue(OwnedDispatch::disarmed(
        KindId(0),
        None,
        Source::NONE,
        MailRef::from(vec![1, 2, 3]),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));
    handler.enqueue(OwnedDispatch::disarmed(
        KindId(0),
        None,
        Source::NONE,
        MailRef::from(vec![4, 5, 6, 7]),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));

    let collected = collected.lock().unwrap();
    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0], vec![1, 2, 3]);
    assert_eq!(collected[1], vec![4, 5, 6, 7]);
    drop(collected);
}

/// Issue iamacoffeepot/aether#848 Phase 1: hand-rolled
/// `impl InboxHandler for MyStruct` compiles and dispatches
/// alongside the blanket-impl path. This is the cap-authoring
/// shape PR 3 will reach for (a struct holding the mpsc Sender);
/// a regression here means caps can't migrate.
#[test]
fn inbox_handler_hand_rolled_impl_dispatches_per_call() {
    use std::sync::mpsc;

    struct ChannelForwarder {
        tx: mpsc::Sender<OwnedDispatch>,
    }
    impl InboxHandler for ChannelForwarder {
        fn enqueue(&self, dispatch: OwnedDispatch) {
            let _ = self.tx.send(dispatch);
        }
    }

    let (tx, rx) = mpsc::channel();
    let handler: Arc<dyn InboxHandler> = Arc::new(ChannelForwarder { tx });
    handler.enqueue(OwnedDispatch::disarmed(
        KindId(42),
        Some("aether.fs".to_owned()),
        Source::NONE,
        MailRef::from(vec![0xAB, 0xCD]),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));

    let received = rx.try_recv().expect("hand-rolled enqueue should send");
    assert_eq!(received.kind, KindId(42));
    assert_eq!(received.payload.into_vec(), vec![0xAB, 0xCD]);
    assert!(rx.try_recv().is_err(), "exactly one enqueue should send exactly one envelope");
}
