//! The request-context half of the inbound frame: a reply's correlation
//! recovers the typed context the request stored, exactly once, and a `Held`
//! the context carries comes back live (ADR-0243 §4).

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use aether_data::wire::{self, HeldClaim, HeldLedger, LedgerEncoder};
use aether_data::{Kind, KindId, MailId, MailboxId, RequestId};

use crate::actor::native::NativeCtx;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::mail::registry::{InboxHandler, OwnedDispatch, Registry};
use crate::mail::{Source, SourceAddr};
use crate::testing::{bare_substrate, boot_authority};

use super::support::{CastOnly, HeldContext, NativeRequestContext, StubActor, TestReply};

/// A terminal test sink (ADR-0094): discharge each dispatch, then forward it.
fn sink(tx: mpsc::Sender<Envelope>) -> Arc<dyn InboxHandler> {
    Arc::new(move |dispatch: OwnedDispatch| {
        dispatch.discharge();
        let _ = tx.send(dispatch);
    })
}

#[test]
fn native_ctx_take_context_consumes_stored_reply_context() {
    let (_registry, mailer) = bare_substrate();
    let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0x00BE_EF10)));
    binding.store_request_context(RequestId(77), NativeRequestContext { value: 9 });

    let reply_source = Source::with_correlation(SourceAddr::None, 77);
    let mut ctx = NativeCtx::new(&binding, reply_source, None, None);

    assert_eq!(ctx.take_context::<NativeRequestContext>(), Some(NativeRequestContext { value: 9 }));
    assert_eq!(ctx.take_context::<NativeRequestContext>(), None);
}

/// Catches a `Held` drop that fires when its context parks, and a claim that
/// fails to rebuild the weak ledger link: the stored debt must drop silently
/// and keep the chain held, and the taken one must answer the caller the
/// hold captured, from a later ctx, echoing that caller's correlation.
#[test]
fn parked_context_drops_silently_and_take_answers_the_original_caller() {
    let (registry, mailer) = bare_substrate();
    let counter = Arc::clone(mailer.trace_handle().settlement_counter());
    let (reply_tx, reply_rx) = mpsc::channel::<Envelope>();
    let caller = registry.register_inbox(&boot_authority(), "test.held_context.caller", sink(reply_tx));
    let (peer_tx, _peer_rx) = mpsc::channel::<Envelope>();
    let peer = Registry::declared_dependency::<StubActor>(registry.register_inbox(
        &boot_authority(),
        "test.held_context.peer",
        sink(peer_tx),
    ));
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x00BE_EF11)));
    let root = MailId::new(MailboxId(0xC2), 1);

    let request = {
        let caller_source = Source::with_correlation(SourceAddr::Component(caller), 77);
        let mut ctx = NativeCtx::new(&binding, caller_source, None, Some(root));
        let (_pending, held) = ctx.hold::<TestReply>();
        ctx.send_to_with_context(peer, &CastOnly { code: 1 }, HeldContext { held, tag: 4 })
    };
    assert_eq!(counter.held_open(root), 1, "the parked debt keeps the caller's chain open");

    {
        let reply_source = Source::with_correlation(SourceAddr::None, request.correlation_id);
        let mut ctx = NativeCtx::new(&binding, reply_source, None, None);
        let context = ctx.take_context::<HeldContext>().expect("the reply takes its stored context");
        assert_eq!(context.tag, 4, "the context's other fields come back beside the debt");
        context.held.answer(&mut ctx, &TestReply { value: 5 });
    }
    assert_eq!(counter.held_open(root), 0, "answering the taken debt releases the hold");

    let reply = reply_rx.recv_timeout(Duration::from_secs(2)).expect("the answer reaches the original caller");
    assert_eq!(reply.sender.correlation_id, 77, "the captured correlation is echoed, not the reply ctx's");
    assert_eq!(TestReply::decode_from_bytes(reply.payload.bytes()), Some(TestReply { value: 5 }));
}

/// A ledger that accepts every park, so a test can read the bytes the
/// request-context table's encoder writes without storing them.
struct AcceptAll;

impl HeldLedger for AcceptAll {
    fn park(&mut self, _ticket: u64, _reply: KindId) -> Result<(), wire::Error> {
        Ok(())
    }

    fn claim(&mut self, _ticket: u64, reply: KindId) -> Result<HeldClaim, wire::Error> {
        Err(wire::Error::HeldUngranted { reply })
    }
}

/// Catches a stray codec path that defuses or claims a debt: a plain encode
/// refuses and leaves the entry held, and a plain decode of the stored bytes
/// refuses and leaves the entry parked for its context's take.
#[test]
fn held_encode_and_decode_outside_the_table_refuse() {
    let (_registry, mailer) = bare_substrate();
    let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0x00BE_EF12)));
    let context = {
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let (_pending, held) = ctx.hold::<TestReply>();
        HeldContext { held, tag: 3 }
    };
    let id = context.held.dispatch_id();

    assert_eq!(
        context.encode_with(&mut Vec::new()),
        Err(wire::Error::HeldUngranted { reply: TestReply::ID }),
        "a plain buffer grants no ledger",
    );
    assert_eq!(binding.dispatch_state_of(id), Some("held"), "a refused encode leaves the entry held");

    let mut ledger = AcceptAll;
    let mut encoder = LedgerEncoder::new(&mut ledger);
    context.encode_with(&mut encoder).expect("a granting encoder writes the ticket");
    let bytes = encoder.into_bytes();
    binding.store_request_context(RequestId(9), context);
    assert_eq!(binding.dispatch_state_of(id), Some("parked"), "storing the context parks its entry");

    assert!(HeldContext::decode_from_bytes(&bytes).is_none(), "a plain decode grants no claim");
    assert_eq!(binding.dispatch_state_of(id), Some("parked"), "a refused decode leaves the entry parked");

    let mut ctx = NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 9), None, None);
    let taken = ctx.take_context::<HeldContext>().expect("the table's take claims the parked entry");
    assert_eq!(binding.dispatch_state_of(id), Some("held"), "the take claims the entry back to held");
    taken.held.answer(&mut ctx, &TestReply { value: 0 });
}
