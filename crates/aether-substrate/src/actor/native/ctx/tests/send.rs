//! What leaves a ctx and under whose chain: a handle, `send_to` or flat
//! `send` inherits the handler's causal chain while a detached one mints a fresh
//! root, and every reply entry point accepts a `Pod`-without-`Serialize` cast
//! kind (ADR-0100).

use std::sync::Arc;

use aether_actor::{Addressable, DependsOn, Manual, One, OutboundReply, Single};
use aether_data::{MailId, MailboxId, RequestId};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::{DeferredReply, Erased, NativeCtx, TaskDone};
use crate::mail::{Source, SourceAddr};

use super::support::{CastOnly, NativeRequestContext, StubActor};

/// ADR-0080 §7 (issue 1802): a handler's send through a proven reference
/// (`ctx.to(&reference).send()`) lands at the reference's id and inherits
/// the in-flight causal chain — the recipient mail lands under the caller's
/// root with the handled mail as its parent — while `send_detached()` opens
/// a fresh chain regardless of the in-flight lineage. The reference is
/// minted for the registered inbox's own position, so the send path is
/// proven off a real route. The buffered send routes at handler end
/// (`NativeCtx`'s `Drop` flush), so the assertions read the routed
/// dispatch's lineage off the registered sink.
#[test]
fn handle_send_inherits_chain_detached_mints_fresh() {
    use crate::mail::registry::{OwnedDispatch, Registry};
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::mpsc;

    let (registry, mailer) = bare_substrate();
    let (tx, rx) = mpsc::channel::<Envelope>();
    let recipient = registry.register_inbox(
        &boot_authority(),
        "test.issue_1802.sink",
        Arc::new(move |dispatch: OwnedDispatch| {
            // Terminal test sink (ADR-0094): discharge before observing.
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );
    let reference = Registry::declared_dependency::<StubActor>(recipient);

    let actor_mailbox = MailboxId(0x00BE_EF02);
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));

    // The chassis-driven chain this handler is running inside.
    let in_flight_root = MailId::new(MailboxId(0xC0), 7);
    let in_flight_mail = MailId::new(MailboxId(0x99), 42);
    let source = Source::with_correlation(SourceAddr::None, 0);

    // Default `send` inherits the caller's chain.
    {
        let ctx = NativeCtx::new(&binding, source, in_flight_mail, in_flight_root);
        ctx.to(&reference).send(&CastOnly { code: 1 });
        // ctx drops here → `flush_outbound` routes the buffered send.
    }
    let inherited = rx.try_recv().expect("default send routed at flush");
    assert_eq!(inherited.recipient, recipient, "send addresses the reference's id");
    assert_eq!(inherited.root, in_flight_root, "send inherits the caller's root");
    assert_eq!(inherited.parent_mail, Some(in_flight_mail), "send's parent is the in-flight mail");
    assert_ne!(inherited.mail_id, in_flight_mail, "the outbound mail_id is fresh");

    // `send_detached` opens a fresh chain despite the in-flight lineage.
    {
        let ctx = NativeCtx::new(&binding, source, in_flight_mail, in_flight_root);
        ctx.to(&reference).send_detached(&CastOnly { code: 2 });
    }
    let detached = rx.try_recv().expect("detached send routed at flush");
    assert!(detached.parent_mail.is_none(), "detached send carries no parent edge");
    assert_eq!(detached.root, detached.mail_id, "detached send is its own root");
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
        let mut ctx = NativeCtx::new(&binding, source, in_flight_mail, in_flight_root);
        ctx.send_to(reference, &CastOnly { code: 1 });
    }
    let sent = rx.try_recv().expect("send_to routed at flush");
    assert_eq!(sent.recipient, recipient, "send_to addresses the reference's id");
    assert_eq!(sent.root, in_flight_root, "send_to inherits the caller's root");
    assert_eq!(sent.parent_mail, Some(in_flight_mail), "send_to's parent is the in-flight mail");

    let inherited_context = NativeRequestContext { value: 21 };
    let inherited_id = {
        let mut ctx = NativeCtx::new(&binding, source, in_flight_mail, in_flight_root);
        let borrowed = &reference;
        ctx.send_to_with_context(borrowed, &CastOnly { code: 2 }, &inherited_context)
    };
    let inherited = rx.try_recv().expect("send_to_with_context routed at flush");
    assert_eq!(inherited.mail_id, inherited_id, "the returned id is the routed mail's");
    assert_eq!(inherited.recipient, recipient, "send_to_with_context addresses the reference's id");
    assert_eq!(inherited.root, in_flight_root, "send_to_with_context inherits the caller's root");
    assert_eq!(inherited.parent_mail, Some(in_flight_mail), "send_to_with_context's parent is the in-flight mail");
    assert_eq!(
        binding.take_request_context::<NativeRequestContext>(RequestId(inherited_id.correlation_id)),
        Some(inherited_context),
        "the context is stored under the routed mail's correlation",
    );

    let detached_context = NativeRequestContext { value: 34 };
    let detached_id = {
        let mut ctx = NativeCtx::new(&binding, source, in_flight_mail, in_flight_root);
        ctx.send_detached_to_with_context(reference.erase(), &CastOnly { code: 3 }, &detached_context)
    };
    let detached = rx.try_recv().expect("send_detached_to_with_context routed at flush");
    assert_eq!(detached.mail_id, detached_id, "the returned id is the routed mail's");
    assert_eq!(detached.recipient, recipient, "send_detached_to_with_context addresses the proof's id");
    assert!(detached.parent_mail.is_none(), "send_detached_to_with_context carries no parent edge");
    assert_eq!(detached.root, detached.mail_id, "send_detached_to_with_context is its own root");
    assert_eq!(
        binding.take_request_context::<NativeRequestContext>(RequestId(detached_id.correlation_id)),
        Some(detached_context),
        "the detached context is stored under the routed mail's correlation",
    );
}

/// The actor the flat-send test's ctx is typed by: it declares the stub actor
/// as a dependency, as `#[actor(depends(StubActor))]` would.
struct Dependent;

impl Addressable for Dependent {
    const NAMESPACE: &'static str = "test.flat_send.dependent";
    type Resolver = One;
}

impl DependsOn<StubActor> for Dependent {}

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
            NativeCtx::new_for_actor(&binding, source, in_flight_mail, in_flight_root);
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
            NativeCtx::new_for_actor(&binding, source, in_flight_mail, in_flight_root);
        ctx.send::<StubActor>(&CastOnly { code: 6 });
    }
    let sent = rx.try_recv().expect("flat send routed at flush");
    assert_eq!(sent.recipient, recipient, "flat send addresses the declared dependency");
    assert_eq!(sent.root, in_flight_root, "flat send inherits the caller's root");
    assert_eq!(sent.parent_mail, Some(in_flight_mail), "flat send's parent is the in-flight mail");

    let context = NativeRequestContext { value: 55 };
    let context_id = {
        let mut ctx: NativeCtx<'_, Dependent, Single> =
            NativeCtx::new_for_actor(&binding, source, in_flight_mail, in_flight_root);
        ctx.send_with_context::<StubActor>(&CastOnly { code: 7 }, &context)
    };
    let with_context = rx.try_recv().expect("flat send_with_context routed at flush");
    assert_eq!(with_context.mail_id, context_id, "the returned id is the routed mail's");
    assert_eq!(with_context.recipient, recipient, "flat send_with_context addresses the declared dependency");
    assert_eq!(with_context.root, in_flight_root, "flat send_with_context inherits the caller's root");
    assert_eq!(with_context.parent_mail, Some(in_flight_mail), "flat send_with_context's parent is the in-flight mail");
    assert_eq!(
        binding.take_request_context::<NativeRequestContext>(RequestId(context_id.correlation_id)),
        Some(context),
        "the context is stored under the routed mail's correlation",
    );
}

/// ADR-0233: the raw-kind verbs are the native door no `ActorMail` bound
/// guards, so each refuses an engine-only kind, returning `MailId::NONE` and
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
        let ctx: NativeCtx<'_, Erased, Single> = NativeCtx::new(&binding, source, MailId::NONE, MailId::NONE);
        let tracked = ctx.send_envelope_tracked_to(sink, MonitorNotice::ID, &notice);
        let detached = ctx.send_envelope_detached_to(sink, MonitorNotice::ID, &notice);
        assert_eq!(tracked, MailId::NONE, "the tracked raw verb refuses engine-only mail");
        assert_eq!(detached, MailId::NONE, "the detached raw verb refuses engine-only mail");

        let control = ctx.send_envelope_detached_to(sink, CastOnly::ID, &CastOnly { code: 8 }.encode_into_bytes());
        assert_ne!(control, MailId::NONE, "an ordinary kind still sends");
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
    ctx.reply_to_target(sender, &CastOnly { code: 4 }, MailId::NONE, None);

    let (task_resolve, task_resolve_with, task_resolve_value, task_resolve_err) = task_dones;
    deferred.reply(task_ctx, &CastOnly { code: 5 });
    task_resolve.resolve(task_ctx);
    task_resolve_with.resolve_with(task_ctx, |output, _context| *output);
    task_resolve_value.resolve_value(task_ctx, &CastOnly { code: 6 });
    task_resolve_err.resolve_err(task_ctx, &CastOnly { code: 7 });
}
