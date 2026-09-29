//! Held replies on the host side (ADR-0243 §6). A guest arm that returns
//! `DISPATCH_HANDLED_HOLD` keeps its reply handle, and its reply-table slot
//! holds the requester's settlement open until the handle is answered. The
//! fixture guest stores kind A's handle, registers its unanswered reply
//! through `held_unanswered_p32`, and returns the hold code; kind B answers
//! the stored handle through `reply_mail_p32`. Each test stands in
//! for the trampoline's dispatcher by bracketing a delivery with its root's
//! `Sent` and `Finished`, and the reply lands in a sink that records its
//! lineage, whether its requester's root was still live when it arrived, and
//! then its own `Finished`.

use std::sync::{Arc, Mutex};

use aether_actor::{DEHYDRATE_HELD_UNSAVED, DISPATCH_HANDLED_HOLD, DISPATCH_HANDLED_RELEASE};
use aether_data::{KindDescriptor, KindId, SchemaType};
use crossbeam_channel::Receiver;

use super::{WAT_REALLOC, ctx_at, instantiate, instantiate_with_ctx};
use crate::actor::native::envelope::Envelope;
use crate::actor::wasm::component::Component;
use crate::actor::wasm::host_fns::{REPLY_KIND_NOT_FOUND, REPLY_OK};
use crate::chassis::settlement::SettlementRegistry;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::{DispatchParts, OwnedDispatch, Registry};
use crate::mail::{MailId, MailRef, MailboxId, Source, SourceAddr};
use crate::runtime::trace::TraceHandle;
use crate::testing::{boot_authority, token_root};

/// The kind whose arm holds its reply.
const KIND_HOLD: KindId = KindId(0xA);
/// The kind whose arm answers the held handle.
const KIND_ANSWER: KindId = KindId(0xB);
/// A kind id no registry knows.
const UNKNOWN_KIND: u64 = 0xBAD;

/// One reply the sink received: its `(mail_id, root, parent_mail)` and
/// whether the requester's root was still live on arrival.
type Received = (Option<MailId>, Option<MailId>, Option<MailId>, bool);

/// A guest whose kind-A arm stores its handle at offset 500 and returns
/// `DISPATCH_HANDLED_HOLD`, first replying inside the dispatch when
/// `reply_in_dispatch` is set, then registering `unanswered_kind` as the
/// held reply's unanswered value when it is `Some`, as the `receive` shim
/// does after the dispatch. Its kind-B arm answers the stored handle with
/// `reply_kind`, stores the status at offset 504, and returns
/// `DISPATCH_HANDLED_RELEASE`.
fn wat_holds(reply_kind: u64, unanswered_kind: Option<u64>, reply_in_dispatch: bool) -> String {
    let hold = KIND_HOLD.0;
    let inline_reply = if reply_in_dispatch {
        format!(
            "(drop (call $reply_mail (local.get 4) (i64.const {reply_kind}) \
             (i32.const 0) (i32.const 0) (i32.const 1) (i64.const 0)))"
        )
    } else {
        String::new()
    };
    let register = unanswered_kind.map_or_else(String::new, |kind| {
        format!("(drop (call $held_unanswered (local.get 4) (i64.const {kind}) (i32.const 0) (i32.const 0)))")
    });
    format!(
        r#"
        (module
            (import "aether" "reply_mail_p32"
                (func $reply_mail (param i32 i64 i32 i32 i32 i64) (result i32)))
            (import "aether" "held_unanswered_p32"
                (func $held_unanswered (param i32 i64 i32 i32) (result i32)))
            (memory (export "memory") 1)
            {WAT_REALLOC}
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                (if (i64.eq (local.get 0) (i64.const {hold}))
                    (then
                        (i32.store (i32.const 500) (local.get 4))
                        {inline_reply}
                        {register}
                        (return (i32.const {DISPATCH_HANDLED_HOLD}))))
                (i32.store (i32.const 504) (call $reply_mail
                    (i32.load (i32.const 500))
                    (i64.const {reply_kind})
                    (i32.const 0)
                    (i32.const 0)
                    (i32.const 1)
                    (i64.const 0)))
                i32.const {DISPATCH_HANDLED_RELEASE}))
        "#
    )
}

/// A held-reply guest wired to a reply sink, with the trace handle its
/// settlement runs through and a registry to observe `Settled` on.
struct Fixture {
    component: Component,
    trace: TraceHandle,
    settlement: Arc<SettlementRegistry>,
    sink: MailboxId,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Fixture {
    /// The guest from [`wat_holds`], registering `test.pong` as its
    /// unanswered reply and answering with it or, when `unknown_reply_kind`
    /// is set, with a kind id no registry knows.
    fn new(reply_in_dispatch: bool, unknown_reply_kind: bool) -> Self {
        Self::build(reply_in_dispatch, unknown_reply_kind, true)
    }

    /// [`Self::new`]'s guest, holding without registering its unanswered
    /// reply when `register` is unset.
    fn build(reply_in_dispatch: bool, unknown_reply_kind: bool, register: bool) -> Self {
        let registry = Arc::new(Registry::new());
        let pong = registry
            .register_kind_with_descriptor(
                &boot_authority(),
                KindDescriptor { name: "test.pong".into(), schema: SchemaType::Unit },
            )
            .expect("register kind");
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let trace = mailer.trace_handle().clone();
        let settlement = Arc::new(SettlementRegistry::new());
        trace.install_settlement_registry(Arc::clone(&settlement));

        let received: Arc<Mutex<Vec<Received>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_received = Arc::clone(&received);
        let sink_trace = trace.clone();
        let sink = registry
            .try_register_inbox(
                &boot_authority(),
                "held_reply_sink",
                Arc::new(move |dispatch: OwnedDispatch| {
                    dispatch.discharge();
                    let live = dispatch.root.is_some_and(|root| sink_trace.settlement_counter().is_live(root));
                    sink_received.lock().unwrap().push((dispatch.mail_id, dispatch.root, dispatch.parent_mail, live));
                    sink_trace.record_finished(dispatch.mail_id, dispatch.root);
                }),
            )
            .expect("register sink");

        let reply_kind = if unknown_reply_kind {
            UNKNOWN_KIND
        } else {
            pong.0
        };
        let ctx = ctx_at(registry, mailer, HubOutbound::disconnected(), MailboxId(0), None);
        let unanswered_kind = register.then_some(pong.0);
        let component = instantiate_with_ctx(&wat_holds(reply_kind, unanswered_kind, reply_in_dispatch), ctx);
        Self { component, trace, settlement, sink, received }
    }

    /// Deliver `kind` as mail `mail_id` on `root` from the reply sink,
    /// bracketed with the root's `Sent` and `Finished` as the dispatcher
    /// records them.
    fn dispatch(&mut self, kind: KindId, mail_id: MailId, root: MailId) -> u32 {
        self.try_dispatch(kind, mail_id, root).expect("deliver")
    }

    /// [`Self::dispatch`], returning a failed delivery.
    fn try_dispatch(&mut self, kind: KindId, mail_id: MailId, root: MailId) -> wasmtime::Result<u32> {
        let sender = Source::with_correlation(SourceAddr::Component(self.sink), 0x5151);
        let parts = DispatchParts {
            sender,
            mail_id: Some(mail_id),
            root: Some(root),
            ..DispatchParts::new(kind, MailRef::from(Vec::new()))
        };
        self.trace.record_sent_inflight(root);
        let rc = self.component.deliver(&Envelope::disarmed_at(parts, MailboxId(0)));
        self.trace.record_finished(Some(mail_id), Some(root));
        rc
    }

    fn subscribe(&self, root: MailId) -> Receiver<()> {
        self.settlement.subscribe_settlement(root)
    }

    fn held_open(&self, root: MailId) -> u32 {
        self.trace.settlement_counter().held_open(root)
    }
}

fn request_id() -> MailId {
    MailId::new(MailboxId(0x99), 42)
}

fn answer_id() -> MailId {
    MailId::new(MailboxId(0x98), 43)
}

/// Catches a host that frees the handle on the hold code, so the requester
/// settles with no reply, or that never releases the hold once answered.
#[test]
fn deliver_holds_the_root_until_the_held_reply() {
    let mut fixture = Fixture::new(false, false);
    let request_root = token_root(7);
    let settled = fixture.subscribe(request_root);

    assert_eq!(fixture.dispatch(KIND_HOLD, request_id(), request_root), DISPATCH_HANDLED_HOLD);

    assert!(settled.try_recv().is_err(), "the held root must not settle before its reply");
    assert_eq!(fixture.held_open(request_root), 1);

    fixture.dispatch(KIND_ANSWER, answer_id(), token_root(8));

    assert_eq!(fixture.component.read_u32(504), REPLY_OK);
    let received = fixture.received.lock().unwrap();
    assert_eq!(received.len(), 1);
    assert!(received[0].3, "the hold must release only after the reply is sent");
    assert!(settled.try_recv().is_ok(), "the reply's release settles the root");
    assert_eq!(fixture.held_open(request_root), 0);
    assert!(!fixture.trace.settlement_counter().is_live(request_root));
}

/// Catches in-flight lineage leaking into a held reply: the answering
/// dispatch runs on another root, and the reply must still name the request.
#[test]
fn held_reply_is_stamped_on_its_original_chain() {
    let mut fixture = Fixture::new(false, false);
    let request_root = token_root(7);
    let answer_root = token_root(8);

    fixture.dispatch(KIND_HOLD, request_id(), request_root);
    fixture.dispatch(KIND_ANSWER, answer_id(), answer_root);

    let received = fixture.received.lock().unwrap();
    let (_, root, parent, _) = received[0];
    assert_eq!(root, Some(request_root));
    assert_eq!(parent, Some(request_id()));
}

/// Catches `hold` arming a slot the guest already answered, which would
/// keep the requester's chain open with no reply left to send.
#[test]
fn answered_in_dispatch_then_hold_leaks_nothing() {
    let mut fixture = Fixture::new(true, false);
    let request_root = token_root(7);
    let settled = fixture.subscribe(request_root);

    assert_eq!(fixture.dispatch(KIND_HOLD, request_id(), request_root), DISPATCH_HANDLED_HOLD);

    assert_eq!(fixture.received.lock().unwrap().len(), 1);
    assert_eq!(fixture.held_open(request_root), 0);
    assert!(settled.try_recv().is_ok());
}

/// Catches `reply_mail_p32` taking the handle before its kind check: a bad
/// kind id would release the hold with the reply unsent.
#[test]
fn a_bad_reply_kind_leaves_the_slot_held() {
    let mut fixture = Fixture::new(false, true);
    let request_root = token_root(7);
    let settled = fixture.subscribe(request_root);

    fixture.dispatch(KIND_HOLD, request_id(), request_root);
    fixture.dispatch(KIND_ANSWER, answer_id(), token_root(8));

    assert_eq!(fixture.component.read_u32(504), REPLY_KIND_NOT_FOUND);
    assert_eq!(fixture.held_open(request_root), 1);
    assert!(settled.try_recv().is_err());
    let handle = fixture.component.read_u32(500);
    assert!(fixture.component.store.data().reply_table.resolve(handle).is_some(), "the handle stays answerable");
}

/// Catches an unload that releases a held slot without sending its
/// registered reply, so the requester's handler never runs; one that sends it
/// after the hold releases, so the root settles before the reply lands; and
/// one stamped on no chain or the wrong one.
#[test]
fn unload_sends_the_registered_reply_before_release() {
    let mut fixture = Fixture::new(false, false);
    let request_root = token_root(7);
    let settled = fixture.subscribe(request_root);

    fixture.dispatch(KIND_HOLD, request_id(), request_root);
    assert!(settled.try_recv().is_err());

    fixture.component.answer_held_at_close();

    let received = fixture.received.lock().unwrap();
    assert_eq!(received.len(), 1, "the registered reply reaches the requester once");
    let (_, root, parent, live) = received[0];
    assert_eq!((root, parent), (Some(request_root), Some(request_id())));
    assert!(live, "the hold releases only after the reply is sent");
    assert!(settled.try_recv().is_ok());
    assert_eq!(fixture.held_open(request_root), 0);
}

/// Catches an engine teardown that answers held replies, which would mail
/// requesters that are closing with the engine, or that keeps their holds.
#[test]
fn engine_teardown_answers_nothing() {
    let mut fixture = Fixture::new(false, false);
    let request_root = token_root(7);
    let settled = fixture.subscribe(request_root);
    fixture.dispatch(KIND_HOLD, request_id(), request_root);

    fixture.component.store.data().binding.signal_engine_teardown();
    fixture.component.answer_held_at_close();

    assert!(fixture.received.lock().unwrap().is_empty());
    assert!(settled.try_recv().is_ok());
    assert_eq!(fixture.held_open(request_root), 0);
}

/// Catches a host that arms a held slot with no registered reply, so an
/// unload or close would leave the requester with nothing to receive.
#[test]
fn a_hold_without_its_unanswered_reply_fails_the_delivery() {
    let mut fixture = Fixture::build(false, false, false);

    let error = fixture.try_dispatch(KIND_HOLD, request_id(), token_root(7)).expect_err("the delivery fails");

    assert!(error.to_string().contains("without registering"), "{error}");
}

/// Catches a host that ignores `on_dehydrate`'s return, letting a replace
/// strand a live held reply.
#[test]
fn dehydrate_status_two_is_a_save_error() {
    let wat = format!(
        r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32 i64 i64) (result i32)
                i32.const 0)
            (func (export "on_dehydrate") (result i32)
                i32.const {DEHYDRATE_HELD_UNSAVED}))
        "#
    );
    let mut component = instantiate(&wat);

    component.on_dehydrate();

    assert!(component.take_save_error().is_some());
}
