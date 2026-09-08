//! What leaves a ctx and under whose chain: a handle send inherits the
//! handler's causal chain while a detached one mints a fresh root, a multi
//! emit addresses the dispatch source detached, and every reply entry point
//! accepts a `Pod`-without-`Serialize` cast kind (ADR-0100).

use std::sync::Arc;

use aether_actor::{Emit, Manual, OutboundReply};
use aether_data::{MailId, MailboxId};

use crate::actor::native::NativeCtx;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::mail::{Source, SourceAddr};

use super::support::{CastOnly, StubActor};

/// ADR-0080 §7 (issue 1802): a handler's `ctx.actor::<R>().send()`
/// inherits the in-flight causal chain — the recipient mail lands
/// under the caller's root with the handled mail as its parent —
/// while `send_detached()` opens a fresh chain regardless of the
/// in-flight lineage. The buffered send routes at handler end
/// (`NativeCtx`'s `Drop` flush), so the assertions read the routed
/// dispatch's lineage off the registered sink.
#[test]
fn handle_send_inherits_chain_detached_mints_fresh() {
    use crate::mail::registry::OwnedDispatch;
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

    let actor_mailbox = MailboxId(0x00BE_EF02);
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));

    // The chassis-driven chain this handler is running inside.
    let in_flight_root = MailId::new(MailboxId(0xC0), 7);
    let in_flight_mail = MailId::new(MailboxId(0x99), 42);
    let source = Source::with_correlation(SourceAddr::None, 0);

    // Default `send` inherits the caller's chain.
    {
        let ctx = NativeCtx::new(&binding, source, in_flight_mail, in_flight_root);
        ctx.actor_at::<StubActor>(recipient).send(&CastOnly { code: 1 });
        // ctx drops here → `flush_outbound` routes the buffered send.
    }
    let inherited = rx.try_recv().expect("default send routed at flush");
    assert_eq!(inherited.root, in_flight_root, "send inherits the caller's root");
    assert_eq!(inherited.parent_mail, Some(in_flight_mail), "send's parent is the in-flight mail");
    assert_ne!(inherited.mail_id, in_flight_mail, "the outbound mail_id is fresh");

    // `send_detached` opens a fresh chain despite the in-flight lineage.
    {
        let ctx = NativeCtx::new(&binding, source, in_flight_mail, in_flight_root);
        ctx.actor_at::<StubActor>(recipient).send_detached(&CastOnly { code: 2 });
    }
    let detached = rx.try_recv().expect("detached send routed at flush");
    assert!(detached.parent_mail.is_none(), "detached send carries no parent edge");
    assert_eq!(detached.root, detached.mail_id, "detached send is its own root");
}

/// ADR-0134: a multi handler's `ctx.emit` addresses the dispatch source
/// and starts a fresh detached chain (no parent edge, its own root);
/// a dispatch with no routable source (`SourceAddr::None`) drops the
/// emission. The buffered send routes at handler end (`NativeCtx`'s
/// `Drop` flush), so the assertions read the routed dispatch off the
/// source-registered sink.
#[test]
fn emit_routes_detached_at_source_and_drops_when_sourceless() {
    use crate::mail::registry::OwnedDispatch;
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::mpsc;

    let (registry, mailer) = bare_substrate();
    let (tx, rx) = mpsc::channel::<Envelope>();
    // The sink is registered under the id the dispatch source names, so
    // a receipt here proves the emit addressed the source.
    let source_id = registry.register_inbox(
        &boot_authority(),
        "test.multi_emit.sink",
        Arc::new(move |dispatch: OwnedDispatch| {
            // Terminal test sink (ADR-0094): discharge before observing.
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );

    let actor_mailbox = MailboxId(0x00BE_EF03);
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));

    // A dispatch whose source is the sink component: emit addresses it.
    let source = Source::with_correlation(SourceAddr::Component(source_id), 0);
    {
        let mut ctx = NativeCtx::new_dispatching(&binding, source, MailId::NONE, MailId::NONE);
        Emit::<CastOnly>::emit(ctx.as_multi::<CastOnly>(), &CastOnly { code: 7 });
        // ctx drops here → `flush_outbound` routes the buffered emit.
    }
    let emitted = rx.try_recv().expect("emit routed at flush");
    assert!(emitted.parent_mail.is_none(), "emit carries no parent edge — it is a detached root");
    assert_eq!(emitted.root, emitted.mail_id, "a detached emit is its own chain root");

    // A sourceless dispatch (`SourceAddr::None`) drops the emission.
    let none_source = Source::with_correlation(SourceAddr::None, 0);
    {
        let mut ctx = NativeCtx::new_dispatching(&binding, none_source, MailId::NONE, MailId::NONE);
        Emit::<CastOnly>::emit(ctx.as_multi::<CastOnly>(), &CastOnly { code: 8 });
    }
    assert!(rx.try_recv().is_err(), "a sourceless emit routes nothing — the emission drops");
}

/// Type-level proof (ADR-0100): a `Pod`-without-`Serialize` cast kind
/// is repliable through every native reply entry point — the bounds
/// relaxed from `K: Kind + serde::Serialize` to `K: Kind`. Never
/// called; the compile is the assertion. If a reply bound regains a
/// `serde::Serialize` half, this stops compiling.
#[allow(dead_code)]
fn _assert_cast_kind_repliable(ctx: &mut NativeCtx<'_, Manual>, sender: Source) {
    OutboundReply::reply(ctx, &CastOnly { code: 2 });
    OutboundReply::reply_to(ctx, sender, &CastOnly { code: 3 });
    ctx.reply_to_target(sender, &CastOnly { code: 4 }, MailId::NONE, None);
}
