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
use aether_data::{Kind, KindDescriptor, KindId, Schema, SchemaType};
use aether_kinds::MonitorNotice;
use crossbeam_channel::Receiver;

use super::{WAT_REALLOC, ctx_at, instantiate, instantiate_with_ctx};
use crate::actor::native::envelope::Envelope;
use crate::actor::wasm::component::Component;
use crate::actor::wasm::host_fns::{REPLY_ENGINE_ONLY_KIND, REPLY_KIND_NOT_FOUND, REPLY_OK, REPLY_OOB};
use crate::chassis::settlement::SettlementRegistry;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::{DispatchParts, OwnedDispatch, PreparedAliasRoute, Registry, RouteContract};
use crate::mail::{MailId, MailRef, MailboxId, Source, SourceAddr};
use crate::runtime::trace::TraceHandle;
use crate::testing::{boot_authority, token_root};

/// The kind whose arm holds its reply.
const KIND_HOLD: KindId = KindId(0xA);
/// The kind whose arm answers the held handle.
const KIND_ANSWER: KindId = KindId(0xB);
/// A kind id no registry knows.
const UNKNOWN_KIND: u64 = 0xBAD;
/// The component's own mailbox.
const OWN: MailboxId = MailboxId(0x0C0C);

/// One reply the sink received.
#[derive(Clone, Copy)]
struct Received {
    root: Option<MailId>,
    parent: Option<MailId>,
    /// Whether the requester's root was still live on arrival.
    live: bool,
    /// The address the reply was sent in the name of.
    from: SourceAddr,
}

/// How the kind-A arm registers its held reply's unanswered value, as the
/// `receive` shim does after the dispatch. The status the host returns is
/// stored at offset 508.
#[derive(Clone, Copy)]
enum Registration {
    /// `test.pong`, on the handle the arm holds.
    Pong,
    /// Nothing: the arm holds without registering.
    Absent,
    /// `test.pong`, on a handle other than the one the arm holds.
    OtherHandle,
    /// Kind `kind` with the payload at `(ptr, len)`.
    Raw { kind: u64, ptr: u32, len: u32 },
}

/// The guest [`wat_holds`] builds.
#[derive(Clone, Copy)]
struct Guest {
    /// The kind-A arm answers its handle inside the dispatch before holding.
    reply_in_dispatch: bool,
    /// The kind-B arm answers with a kind id no registry knows rather than
    /// `test.pong`.
    unknown_reply_kind: bool,
    registration: Registration,
}

impl Default for Guest {
    fn default() -> Self {
        Self { reply_in_dispatch: false, unknown_reply_kind: false, registration: Registration::Pong }
    }
}

/// The WAT of `guest`, whose replies and registrations name `pong` as
/// `test.pong`: its kind-A arm stores its handle at offset 500 and returns
/// `DISPATCH_HANDLED_HOLD`, first replying inside the dispatch when
/// `reply_in_dispatch` is set, then registering per its [`Registration`].
/// Its kind-B arm answers the stored handle, stores the status at offset
/// 504, and returns `DISPATCH_HANDLED_RELEASE`.
fn wat_holds(guest: Guest, pong: u64) -> String {
    let hold = KIND_HOLD.0;
    let reply_kind = if guest.unknown_reply_kind {
        UNKNOWN_KIND
    } else {
        pong
    };
    let inline_reply = if guest.reply_in_dispatch {
        format!(
            "(drop (call $reply_mail (local.get 4) (i64.const {reply_kind}) \
             (i32.const 0) (i32.const 0) (i32.const 1) (i64.const 0)))"
        )
    } else {
        String::new()
    };
    let register = |handle: &str, kind: u64, ptr: u32, len: u32| {
        format!(
            "(i32.store (i32.const 508) (call $held_unanswered {handle} (i64.const {kind}) \
             (i32.const {ptr}) (i32.const {len})))"
        )
    };
    let register = match guest.registration {
        Registration::Pong => register("(local.get 4)", pong, 0, 0),
        Registration::Absent => String::new(),
        Registration::OtherHandle => register("(i32.add (local.get 4) (i32.const 1))", pong, 0, 0),
        Registration::Raw { kind, ptr, len } => register("(local.get 4)", kind, ptr, len),
    };
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
    /// The component [`OWN`] hosting `guest`, with `test.pong` and
    /// [`MonitorNotice`] registered, the latter an engine-only kind.
    fn new(guest: Guest) -> Self {
        let registry = Arc::new(Registry::new());
        let pong = registry
            .register_kind_with_descriptor(
                &boot_authority(),
                KindDescriptor { name: "test.pong".into(), schema: SchemaType::Unit },
            )
            .expect("register kind");
        registry
            .register_kind_with_descriptor(
                &boot_authority(),
                KindDescriptor { name: <MonitorNotice as Kind>::NAME.into(), schema: MonitorNotice::SCHEMA },
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
                    let (root, parent, from) = (dispatch.root, dispatch.parent_mail, dispatch.sender.addr);
                    sink_received.lock().unwrap().push(Received { root, parent, live, from });
                    sink_trace.record_finished(dispatch.mail_id, dispatch.root);
                }),
            )
            .expect("register sink");

        let ctx = ctx_at(registry, mailer, HubOutbound::disconnected(), OWN, None);
        let component = instantiate_with_ctx(&wat_holds(guest, pong.0), ctx);
        Self { component, trace, settlement, sink, received }
    }

    /// Deliver `kind` as mail `mail_id` on `root` from the reply sink to the
    /// component, bracketed with the root's `Sent` and `Finished` as the
    /// dispatcher records them.
    fn dispatch(&mut self, kind: KindId, mail_id: MailId, root: MailId) -> u32 {
        self.try_dispatch_to(OWN, kind, mail_id, root).expect("deliver")
    }

    /// [`Self::dispatch`], returning a failed delivery.
    fn try_dispatch(&mut self, kind: KindId, mail_id: MailId, root: MailId) -> wasmtime::Result<u32> {
        self.try_dispatch_to(OWN, kind, mail_id, root)
    }

    /// [`Self::try_dispatch`] routed to `recipient`: the component, or an
    /// inline child it hosts.
    fn try_dispatch_to(
        &mut self,
        recipient: MailboxId,
        kind: KindId,
        mail_id: MailId,
        root: MailId,
    ) -> wasmtime::Result<u32> {
        let sender = Source::with_correlation(SourceAddr::Component(self.sink), 0x5151);
        let parts = DispatchParts {
            sender,
            mail_id: Some(mail_id),
            root: Some(root),
            ..DispatchParts::new(kind, MailRef::from(Vec::new()))
        };
        self.trace.record_sent_inflight(root);
        let rc = self.component.deliver(&Envelope::disarmed_at(parts, recipient));
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
    let mut fixture = Fixture::new(Guest::default());
    let request_root = token_root(7);
    let settled = fixture.subscribe(request_root);

    assert_eq!(fixture.dispatch(KIND_HOLD, request_id(), request_root), DISPATCH_HANDLED_HOLD);

    assert!(settled.try_recv().is_err(), "the held root must not settle before its reply");
    assert_eq!(fixture.held_open(request_root), 1);

    fixture.dispatch(KIND_ANSWER, answer_id(), token_root(8));

    assert_eq!(fixture.component.read_u32(504), REPLY_OK);
    let received = fixture.received.lock().unwrap();
    assert_eq!(received.len(), 1);
    assert!(received[0].live, "the hold must release only after the reply is sent");
    assert!(settled.try_recv().is_ok(), "the reply's release settles the root");
    assert_eq!(fixture.held_open(request_root), 0);
    assert!(!fixture.trace.settlement_counter().is_live(request_root));
}

/// Catches in-flight lineage leaking into a held reply: the answering
/// dispatch runs on another root, and the reply must still name the request.
#[test]
fn held_reply_is_stamped_on_its_original_chain() {
    let mut fixture = Fixture::new(Guest::default());
    let request_root = token_root(7);
    let answer_root = token_root(8);

    fixture.dispatch(KIND_HOLD, request_id(), request_root);
    fixture.dispatch(KIND_ANSWER, answer_id(), answer_root);

    let received = fixture.received.lock().unwrap();
    assert_eq!(received[0].root, Some(request_root));
    assert_eq!(received[0].parent, Some(request_id()));
}

/// Catches `hold` arming a slot the guest already answered, which would
/// keep the requester's chain open with no reply left to send.
#[test]
fn answered_in_dispatch_then_hold_leaks_nothing() {
    let mut fixture = Fixture::new(Guest { reply_in_dispatch: true, ..Guest::default() });
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
    let mut fixture = Fixture::new(Guest { unknown_reply_kind: true, ..Guest::default() });
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
    let mut fixture = Fixture::new(Guest::default());
    let request_root = token_root(7);
    let settled = fixture.subscribe(request_root);

    fixture.dispatch(KIND_HOLD, request_id(), request_root);
    assert!(settled.try_recv().is_err());

    fixture.component.answer_held_at_close();

    let received = fixture.received.lock().unwrap();
    assert_eq!(received.len(), 1, "the registered reply reaches the requester once");
    let Received { root, parent, live, from } = received[0];
    assert_eq!((root, parent), (Some(request_root), Some(request_id())));
    assert!(live, "the hold releases only after the reply is sent");
    assert_eq!(from, SourceAddr::Component(OWN), "sent in the name of the component that held");
    assert!(settled.try_recv().is_ok());
    assert_eq!(fixture.held_open(request_root), 0);
}

/// Catches an engine teardown that answers held replies, which would mail
/// requesters that are closing with the engine, or that keeps their holds.
#[test]
fn engine_teardown_answers_nothing() {
    let mut fixture = Fixture::new(Guest::default());
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
    let mut fixture = Fixture::new(Guest { registration: Registration::Absent, ..Guest::default() });

    let error = fixture.try_dispatch(KIND_HOLD, request_id(), token_root(7)).expect_err("the delivery fails");

    assert!(error.to_string().contains("without registering"), "{error}");
}

/// Catches a host that arms a held slot from a registration made for
/// another handle, so the requester would receive a reply registered for a
/// different request, or none.
#[test]
fn a_registration_for_another_handle_fails_the_delivery() {
    let mut fixture = Fixture::new(Guest { registration: Registration::OtherHandle, ..Guest::default() });

    let error = fixture.try_dispatch(KIND_HOLD, request_id(), token_root(7)).expect_err("the delivery fails");

    assert_eq!(fixture.component.read_u32(508), REPLY_OK, "the registration itself is accepted");
    assert!(error.to_string().contains("without registering"), "{error}");
}

/// Catches `held_unanswered_p32` accepting a registration the host could not
/// send at close: an engine-only kind a guest would forge as its reply, a
/// kind the requester could not decode, or a payload outside guest memory.
/// Each refusal returns its status and stores nothing, so the hold then
/// fails the delivery.
#[test]
fn a_refused_registration_leaves_the_hold_unregistered() {
    let pages_end = 65_536;
    let refusals = [
        (Registration::Raw { kind: MonitorNotice::ID.0, ptr: 0, len: 0 }, REPLY_ENGINE_ONLY_KIND),
        (Registration::Raw { kind: UNKNOWN_KIND, ptr: 0, len: 0 }, REPLY_KIND_NOT_FOUND),
        (Registration::Raw { kind: KIND_ANSWER.0, ptr: pages_end, len: 1 }, REPLY_OOB),
    ];

    for (registration, status) in refusals {
        let mut fixture = Fixture::new(Guest { registration, ..Guest::default() });

        let error = fixture.try_dispatch(KIND_HOLD, request_id(), token_root(7)).expect_err("the delivery fails");

        assert_eq!(fixture.component.read_u32(508), status);
        assert!(error.to_string().contains("without registering"), "{error}");
    }
}

/// Catches an unanswered reply sent in the component's name when an inline
/// child held it: the guest's own answer comes from the child, so the
/// requester would see a different sender for the same request.
#[test]
fn an_inline_childs_unanswered_reply_comes_from_the_child() {
    let mut fixture = Fixture::new(Guest::default());
    let child = MailboxId(0x0C0D);
    fixture.component.store.data_mut().stage_alias(PreparedAliasRoute::new(
        child,
        "held-inline-child",
        OWN,
        RouteContract::empty(),
    ));

    fixture.try_dispatch_to(child, KIND_HOLD, request_id(), token_root(7)).expect("deliver");
    fixture.component.answer_held_at_close();

    let received = fixture.received.lock().unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].from, SourceAddr::Component(child));
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
