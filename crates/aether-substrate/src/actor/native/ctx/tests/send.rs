//! What leaves a ctx and under whose chain: `send_to` or flat `send` inherits
//! the handler's causal chain while a detached one mints a fresh root, and
//! every reply entry point accepts a `Pod`-without-`Serialize` cast kind
//! (ADR-0100).

use std::sync::Arc;

use aether_actor::{Addressable, MailSender, Manual, OutboundReply, Single, Undeclared};
use aether_data::{MailId, MailboxId, RequestId};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::{DeferredReply, Erased, NativeActor, NativeCtx, NativeInitCtx, TaskDone};
use crate::chassis::error::BootError;
use crate::mail::{Source, SourceAddr};

use super::support::{CastOnly, NativeRequestContext, StubActor};

#[aether_actor::protocol]
trait CastRelay {
    fn cast(mail: CastOnly) -> Undeclared;
}

struct ManualCastRelay {
    received: u32,
}

#[aether_actor::actor(instanced, root)]
impl NativeActor for ManualCastRelay {
    const NAMESPACE: &'static str = "test.manual_cast_relay";
    type Config = ();

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { received: 0 })
    }

    #[handler::manual]
    fn on_cast(&mut self, _ctx: &mut NativeCtx<'_, Self, Manual>, _mail: CastOnly) {
        self.received += 1;
    }
}

/// ADR-0232 §1: the flat `send_to` family sends through a held reference.
/// `send_to` and `send_to_with_context` land at the reference's id under the
/// in-flight root with the handled mail as parent, while
/// `send_detached_to_with_context` roots its own chain. Both context variants
/// store the context under the correlation of the mail they routed, which is
/// how the bloomery driver's replies find their way back to the continuation
/// that sent them. The legs cover all three `Target` impls: a typed reference
/// by value, a borrow of one, and an erased proof.
#[test]
fn send_to_family_inherits_or_detaches_and_stores_context() {
    use crate::mail::registry::{OwnedDispatch, Registry};
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::mpsc;

    let (registry, mailer) = bare_substrate();
    let (tx, rx) = mpsc::channel::<Envelope>();
    let recipient = registry.register_inbox(
        &boot_authority(),
        "test.send_to_family.sink",
        Arc::new(move |dispatch: OwnedDispatch| {
            // Terminal test sink (ADR-0094): discharge before observing.
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );
    let reference = Registry::declared_dependency::<StubActor>(recipient);

    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x00BE_EF03)));
    let in_flight_root = MailId::new(MailboxId(0xC1), 8);
    let in_flight_mail = MailId::new(MailboxId(0x9A), 43);
    let source = Source::with_correlation(SourceAddr::None, 0);

    {
        let mut ctx = NativeCtx::new(&binding, source, Some(in_flight_mail), Some(in_flight_root));
        ctx.send_to(reference, &CastOnly { code: 1 });
    }
    let sent = rx.try_recv().expect("send_to routed at flush");
    assert_eq!(sent.recipient, recipient, "send_to addresses the reference's id");
    assert_eq!(sent.root, Some(in_flight_root), "send_to inherits the caller's root");
    assert_eq!(sent.parent_mail, Some(in_flight_mail), "send_to's parent is the in-flight mail");

    let inherited_context = NativeRequestContext { value: 21 };
    let inherited_id = {
        let mut ctx = NativeCtx::new(&binding, source, Some(in_flight_mail), Some(in_flight_root));
        let borrowed = &reference;
        ctx.send_to_with_context(borrowed, &CastOnly { code: 2 }, &inherited_context)
    };
    let inherited = rx.try_recv().expect("send_to_with_context routed at flush");
    assert_eq!(inherited.mail_id, Some(inherited_id), "the returned id is the routed mail's");
    assert_eq!(inherited.recipient, recipient, "send_to_with_context addresses the reference's id");
    assert_eq!(inherited.root, Some(in_flight_root), "send_to_with_context inherits the caller's root");
    assert_eq!(inherited.parent_mail, Some(in_flight_mail), "send_to_with_context's parent is the in-flight mail");
    assert_eq!(
        binding.take_request_context::<NativeRequestContext>(RequestId(inherited_id.correlation_id)),
        Some(inherited_context),
        "the context is stored under the routed mail's correlation",
    );

    let detached_context = NativeRequestContext { value: 34 };
    let detached_id = {
        let mut ctx = NativeCtx::new(&binding, source, Some(in_flight_mail), Some(in_flight_root));
        ctx.send_detached_to_with_context(reference.erase(), &CastOnly { code: 3 }, &detached_context)
    };
    let detached = rx.try_recv().expect("send_detached_to_with_context routed at flush");
    assert_eq!(detached.mail_id, Some(detached_id), "the returned id is the routed mail's");
    assert_eq!(detached.recipient, recipient, "send_detached_to_with_context addresses the proof's id");
    assert!(detached.parent_mail.is_none(), "send_detached_to_with_context carries no parent edge");
    assert_eq!(detached.root, detached.mail_id, "send_detached_to_with_context is its own root");
    assert_eq!(
        binding.take_request_context::<NativeRequestContext>(RequestId(detached_id.correlation_id)),
        Some(detached_context),
        "the detached context is stored under the routed mail's correlation",
    );
}

/// ADR-0231 §9: a relay accepts the forwarded kind only through the target's
/// typed row, while preserving the original requester's reply destination and
/// the inbound parent/root. The protocol row is manual, so this also catches
/// an implementation that accidentally rejects `Undeclared` relay targets.
#[test]
fn forward_to_typed_manual_protocol_preserves_reply_target_and_lineage() {
    use crate::mail::registry::{OwnedDispatch, Registry};
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::mpsc;

    let (registry, mailer) = bare_substrate();
    let (tx, rx) = mpsc::channel::<Envelope>();
    let recipient = registry.register_inbox(
        &boot_authority(),
        "test.forward_to_typed.sink",
        Arc::new(move |dispatch: OwnedDispatch| {
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );
    let target = Registry::declared_dependency::<ManualCastRelay>(recipient).narrow::<CastRelay>();
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x00BE_EF07)));
    let root = MailId::new(MailboxId(0xC4), 11);
    let parent = MailId::new(MailboxId(0x9D), 46);
    let requester = Source::with_correlation(SourceAddr::Component(MailboxId(0x1234)), 73);

    {
        let ctx = NativeCtx::new_dispatching(&binding, requester, Some(parent), Some(root));
        ctx.forward_to(target, &CastOnly { code: 9 });
    }

    let forwarded = rx.try_recv().expect("forward_to routed at flush");
    assert_eq!(forwarded.recipient, recipient, "forward_to addresses the typed target");
    assert_eq!(forwarded.sender, requester, "the target replies directly to the original requester");
    assert_eq!(forwarded.parent_mail, Some(parent), "the forwarded mail remains a child of the inbound");
    assert_eq!(forwarded.root, Some(root), "the forwarded mail remains in the inbound chain");
}

/// The inherent typed detached verb returns the id minted by its one buffered
/// push, starts a parentless chain rooted at that id, and stamps this actor as
/// the reply destination. The compatibility trait delegates to the same path
/// while retaining its shared `()` return signature.
#[test]
fn typed_detached_send_returns_emitted_id_and_mail_sender_delegates() {
    use crate::mail::registry::{OwnedDispatch, Registry};
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::mpsc;

    let (registry, mailer) = bare_substrate();
    let (tx, rx) = mpsc::channel::<Envelope>();
    let recipient = registry.register_inbox(
        &boot_authority(),
        "test.typed_detached.sink",
        Arc::new(move |dispatch: OwnedDispatch| {
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );
    let actor = Registry::declared_dependency::<ManualCastRelay>(recipient);
    let target = actor.narrow::<CastRelay>();
    let sender = MailboxId(0x00BE_EF08);
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), sender));
    let inbound_root = MailId::new(MailboxId(0x44), 19);
    let inbound_parent = MailId::new(MailboxId(0x55), 23);

    let emitted_id = {
        let mut ctx: NativeCtx<'_, Erased, Single> =
            NativeCtx::new(&binding, Source::NONE, Some(inbound_parent), Some(inbound_root));
        ctx.send_detached_to(target, &CastOnly { code: 10 })
    };
    let detached = rx.try_recv().expect("typed detached send routed at flush");
    assert_eq!(detached.mail_id, Some(emitted_id), "the inherent verb returns the push's id");
    assert!(detached.parent_mail.is_none(), "detached mail has no parent");
    assert_eq!(detached.root, Some(emitted_id), "detached mail roots its own chain");
    assert_eq!(
        detached.sender,
        Source::with_correlation(SourceAddr::Component(sender), emitted_id.correlation_id),
        "replies address the actor that sent the detached mail",
    );

    {
        let mut ctx: NativeCtx<'_, Erased, Single> =
            NativeCtx::new(&binding, Source::NONE, Some(inbound_parent), Some(inbound_root));
        MailSender::send_detached_to(&mut ctx, actor.erase(), &CastOnly { code: 11 });
    }
    let delegated = rx.try_recv().expect("MailSender detached send routed at flush");
    assert_eq!(delegated.recipient, recipient, "compatibility delegation keeps the target");
    assert!(delegated.parent_mail.is_none(), "compatibility delegation stays detached");
    assert_eq!(delegated.root, delegated.mail_id, "compatibility delegation roots the emitted mail");
}

/// A payload encoded off the sending thread reaches a manual protocol row as
/// the kind its `Encoded<K>` names, byte for byte, on a fresh chain. Catches
/// the verb stamping a kind other than `K`, sending bytes other than the
/// encode, or inheriting the handler's chain the way `send_to` does.
#[test]
fn encoded_detached_send_delivers_the_encoded_kind_through_a_manual_protocol_row() {
    use crate::mail::registry::{OwnedDispatch, Registry};
    use crate::testing::{bare_substrate, boot_authority};
    use aether_data::{Encoded, Kind};
    use std::sync::mpsc;

    let (registry, mailer) = bare_substrate();
    let (tx, rx) = mpsc::channel::<Envelope>();
    let recipient = registry.register_inbox(
        &boot_authority(),
        "test.encoded_detached.sink",
        Arc::new(move |dispatch: OwnedDispatch| {
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );
    let target = Registry::declared_dependency::<ManualCastRelay>(recipient).narrow::<CastRelay>();
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x00BE_EF09)));
    let payload = Encoded::new(&CastOnly { code: 12 });

    let emitted_id = {
        let ctx: NativeCtx<'_, Erased, Single> = NativeCtx::new(
            &binding,
            Source::NONE,
            Some(MailId::new(MailboxId(0x66), 29)),
            Some(MailId::new(MailboxId(0x77), 31)),
        );
        ctx.send_encoded_detached_to(target, &payload).expect("an ordinary kind sends")
    };

    let sent = rx.try_recv().expect("the encoded send routed at flush");
    assert_eq!(sent.recipient, recipient, "the send addresses the protocol reference's target");
    assert_eq!(sent.kind, CastOnly::ID, "the kind is the one the payload was encoded as");
    assert_eq!(bytemuck::pod_read_unaligned::<CastOnly>(sent.payload.bytes()).code, 12, "the payload decodes as sent");
    assert_eq!(sent.mail_id, Some(emitted_id), "the verb returns the push's id");
    assert!(sent.parent_mail.is_none(), "the encoded send carries no parent edge");
    assert_eq!(sent.root, Some(emitted_id), "the encoded send roots its own chain");
}

/// The actor the flat-send test's ctx is typed by: it declares the stub actor
/// as a dependency.
struct Dependent;

#[aether_actor::actor(depends(StubActor))]
impl NativeActor for Dependent {
    const NAMESPACE: &'static str = "test.flat_send.dependent";
    type Config = ();

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut NativeCtx<'_>, _env: &Envelope) {
        let _ = self;
    }
}

/// ADR-0232 §1–§2: the flat `send_detached::<R>` on a ctx typed by an actor
/// that declares `R` lands at the position the dependency's proof points to,
/// and roots a fresh chain despite the handler's in-flight lineage
/// (ADR-0080 §7). The sink is registered at the stub actor's own namespace,
/// so a verb that resolved any other position, or inherited the running
/// chain, fails here rather than only in the fleet proxy's reports.
#[test]
fn flat_send_detached_reaches_the_declared_dependency_on_a_fresh_chain() {
    use crate::mail::registry::OwnedDispatch;
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::mpsc;

    let (registry, mailer) = bare_substrate();
    let (tx, rx) = mpsc::channel::<Envelope>();
    let recipient = registry.register_inbox(
        &boot_authority(),
        StubActor::NAMESPACE,
        Arc::new(move |dispatch: OwnedDispatch| {
            // Terminal test sink (ADR-0094): discharge before observing.
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );

    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x00BE_EF04)));
    let in_flight_root = MailId::new(MailboxId(0xC2), 9);
    let in_flight_mail = MailId::new(MailboxId(0x9B), 44);
    let source = Source::with_correlation(SourceAddr::None, 0);

    {
        let mut ctx: NativeCtx<'_, Dependent, Single> =
            NativeCtx::new_for_actor(&binding, source, Some(in_flight_mail), Some(in_flight_root));
        ctx.send_detached::<StubActor>(&CastOnly { code: 5 });
    }
    let detached = rx.try_recv().expect("flat send_detached routed at flush");
    assert_eq!(detached.recipient, recipient, "flat send_detached addresses the declared dependency");
    assert!(detached.parent_mail.is_none(), "flat send_detached carries no parent edge");
    assert_eq!(detached.root, detached.mail_id, "flat send_detached is its own root");
}

/// ADR-0232 §1: the flat `send::<R>` and `send_with_context::<R>` on a ctx
/// typed by an actor that declares `R` land at the position the dependency's
/// proof points to, under the handler's in-flight root with the handled mail
/// as parent (ADR-0080 §7), and `send_with_context` stores its context under
/// the routed mail's correlation. A body copied from `send_detached` with no
/// lineage, a wrong recipient, or a context stored under another correlation
/// fails here rather than only in the audio and text caps' fs round trips.
#[test]
fn flat_send_and_send_with_context_reach_the_declared_dependency_on_the_handlers_chain() {
    use crate::mail::registry::OwnedDispatch;
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::mpsc;

    let (registry, mailer) = bare_substrate();
    let (tx, rx) = mpsc::channel::<Envelope>();
    let recipient = registry.register_inbox(
        &boot_authority(),
        StubActor::NAMESPACE,
        Arc::new(move |dispatch: OwnedDispatch| {
            // Terminal test sink (ADR-0094): discharge before observing.
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );

    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x00BE_EF05)));
    let in_flight_root = MailId::new(MailboxId(0xC3), 10);
    let in_flight_mail = MailId::new(MailboxId(0x9C), 45);
    let source = Source::with_correlation(SourceAddr::None, 0);

    {
        let mut ctx: NativeCtx<'_, Dependent, Single> =
            NativeCtx::new_for_actor(&binding, source, Some(in_flight_mail), Some(in_flight_root));
        ctx.send::<StubActor>(&CastOnly { code: 6 });
    }
    let sent = rx.try_recv().expect("flat send routed at flush");
    assert_eq!(sent.recipient, recipient, "flat send addresses the declared dependency");
    assert_eq!(sent.root, Some(in_flight_root), "flat send inherits the caller's root");
    assert_eq!(sent.parent_mail, Some(in_flight_mail), "flat send's parent is the in-flight mail");

    let context = NativeRequestContext { value: 55 };
    let context_id = {
        let mut ctx: NativeCtx<'_, Dependent, Single> =
            NativeCtx::new_for_actor(&binding, source, Some(in_flight_mail), Some(in_flight_root));
        ctx.send_with_context::<StubActor>(&CastOnly { code: 7 }, &context)
    };
    let with_context = rx.try_recv().expect("flat send_with_context routed at flush");
    assert_eq!(with_context.mail_id, Some(context_id), "the returned id is the routed mail's");
    assert_eq!(with_context.recipient, recipient, "flat send_with_context addresses the declared dependency");
    assert_eq!(with_context.root, Some(in_flight_root), "flat send_with_context inherits the caller's root");
    assert_eq!(with_context.parent_mail, Some(in_flight_mail), "flat send_with_context's parent is the in-flight mail");
    assert_eq!(
        binding.take_request_context::<NativeRequestContext>(RequestId(context_id.correlation_id)),
        Some(context),
        "the context is stored under the routed mail's correlation",
    );
}

/// ADR-0233: the raw-kind verbs are the native door no `ActorMail` bound
/// guards, so each refuses an engine-only kind, returning no mail id and
/// routing nothing, while an ordinary kind through the same verb still
/// arrives. Catches a native actor forging a departure notice by its id.
#[test]
fn raw_send_of_an_engine_only_kind_is_refused() {
    use aether_data::Kind;
    use aether_kinds::MonitorNotice;

    use crate::mail::registry::OwnedDispatch;
    use crate::testing::{bare_substrate, registered_ref};
    use std::sync::mpsc;

    let (registry, mailer) = bare_substrate();
    let (tx, rx) = mpsc::channel::<Envelope>();
    let sink = registered_ref(
        &registry,
        "test.engine_only.sink",
        Arc::new(move |dispatch: OwnedDispatch| {
            // Terminal test sink (ADR-0094): discharge before observing.
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x00BE_EF06)));
    let source = Source::with_correlation(SourceAddr::None, 0);
    let notice = MonitorNotice.encode_into_bytes();

    {
        let ctx: NativeCtx<'_, Erased, Single> = NativeCtx::new(&binding, source, None, None);
        let tracked = ctx.send_envelope_tracked_to(sink, MonitorNotice::ID, &notice);
        let detached = ctx.send_envelope_detached_to(sink, MonitorNotice::ID, &notice);
        assert_eq!(tracked, None, "the tracked raw verb refuses engine-only mail");
        assert_eq!(detached, None, "the detached raw verb refuses engine-only mail");

        let control = ctx.send_envelope_detached_to(sink, CastOnly::ID, &CastOnly { code: 8 }.encode_into_bytes());
        assert!(control.is_some(), "an ordinary kind still sends");
    }

    let arrived = rx.try_recv().expect("the ordinary kind routed at flush");
    assert_eq!(arrived.kind, CastOnly::ID, "only the ordinary kind reaches the sink");
    assert!(rx.try_recv().is_err(), "no engine-only mail reached the sink");
}

/// One `TaskDone<CastOnly, ()>` per `resolve*` method, bundled into a tuple
/// so `_assert_cast_kind_repliable`'s parameter count stays under clippy's
/// `too_many_arguments` threshold without a suppression.
type CastOnlyTaskDones =
    (TaskDone<CastOnly, ()>, TaskDone<CastOnly, ()>, TaskDone<CastOnly, ()>, TaskDone<CastOnly, ()>);

/// Type-level proof (ADR-0100): a `Pod`-without-`Serialize` cast kind
/// is repliable through every native reply entry point — the bounds
/// relaxed from `K: Kind + serde::Serialize` to `K: Kind` (now
/// `K: ActorMail`, ADR-0233). Never
/// called; the compile is the assertion. If a reply bound regains a
/// `serde::Serialize` half, this stops compiling. Covers the direct
/// entry points (`OutboundReply::reply`, `reply_to`, `reply_to_target`)
/// and the offload reply paths (`DeferredReply::reply` and every
/// `TaskDone::resolve*`) alike.
#[allow(dead_code)]
fn _assert_cast_kind_repliable(
    ctx: &mut NativeCtx<'_, Erased, Manual>,
    sender: Source,
    deferred: DeferredReply,
    task_dones: CastOnlyTaskDones,
    task_ctx: &mut NativeCtx<'_, Erased, Single>,
) {
    OutboundReply::reply(ctx, &CastOnly { code: 2 });
    OutboundReply::reply_to(ctx, sender, &CastOnly { code: 3 });
    ctx.reply_to_target(sender, &CastOnly { code: 4 }, None, None);

    let (task_resolve, task_resolve_with, task_resolve_value, task_resolve_err) = task_dones;
    deferred.reply(task_ctx, &CastOnly { code: 5 });
    task_resolve.resolve(task_ctx);
    task_resolve_with.resolve_with(task_ctx, |output, _context| *output);
    task_resolve_value.resolve_value(task_ctx, &CastOnly { code: 6 });
    task_resolve_err.resolve_err(task_ctx, &CastOnly { code: 7 });
}
