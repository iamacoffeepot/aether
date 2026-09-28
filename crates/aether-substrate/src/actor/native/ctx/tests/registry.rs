//! Registry reads off a ctx: a proven reference answers its actor's
//! canonical path, before and after that actor departs, and so does the
//! sender reference a real dispatch stamps. A registry batch staged from a
//! held reply answers the caller that reply captured.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use aether_data::{ErasedActorPath, Kind, MailId, MailboxId};

use crate::actor::native::{
    DispatchId, NativeBinding, NativeCtx, RegistryBatch, RegistryBatchResult, TaskCompletionWake,
};
use crate::actor::registry::ActorRegistry;
use crate::config::RingCapacities;
use crate::mail::registry::{InboxHandler, OwnedDispatch};
use crate::mail::{Source, SourceAddr};
use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
use crate::scheduler::{Pool, PoolConfig};
use crate::testing::{bare_substrate, boot_authority, drop_ref, registered_binding, registered_ref};

use super::support::{CastOnly, TestReply};

fn discharging() -> Arc<dyn InboxHandler> {
    Arc::new(|dispatch: OwnedDispatch| dispatch.discharge())
}

/// The path is read from a route of any lifecycle, so a peer that has since
/// departed is still named: the dropped-mailbox case is the one a log line
/// most needs a name for, and a lookup that read only `Live` routes would
/// blank it.
#[test]
fn actor_path_names_a_peer_before_and_after_it_departs() {
    let (registry, mailer) = bare_substrate();
    let (binding, _caller) = registered_binding(&registry, &mailer, "test.native.path_host", discharging());
    let peer_name = "test.native.path_peer";
    let peer = registered_ref(&registry, peer_name, discharging());
    let expected = ErasedActorPath::new(peer_name).expect("the peer name is a canonical path");
    let ctx = NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 0), None, None);

    assert_eq!(ctx.actor_path(peer), expected);

    drop_ref(&registry, peer);

    assert_eq!(ctx.actor_path(peer), expected);
}

/// `actor_path` panics on a reference with no route record, so every
/// reference `sender` hands a handler must name one. A real send stamps the
/// sending actor's position on the envelope; the receiving ctx's `sender`
/// proves exactly that actor, and its path answers the sender's name — while
/// the sender is live and after it departs.
#[test]
fn actor_path_names_the_sender_a_real_dispatch_stamps() {
    let (registry, mailer) = bare_substrate();
    let sender_name = "test.native.stamp_sender";
    let (sender_binding, sender) = registered_binding(&registry, &mailer, sender_name, discharging());
    let (tx, rx) = mpsc::channel::<OwnedDispatch>();
    let (receiver_binding, receiver) = registered_binding(
        &registry,
        &mailer,
        "test.native.stamp_receiver",
        Arc::new(move |dispatch: OwnedDispatch| {
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );
    let expected = ErasedActorPath::new(sender_name).expect("the sender name is a canonical path");

    {
        let mut ctx = NativeCtx::new(&sender_binding, Source::with_correlation(SourceAddr::None, 0), None, None);
        ctx.send_to(receiver, &CastOnly { code: 3 });
    }
    let delivered = rx.try_recv().expect("the send routes at ctx flush");
    let ctx = NativeCtx::new(&receiver_binding, delivered.sender, delivered.mail_id, delivered.root);
    let stamped = ctx.sender().expect("a routed actor's send carries its sender");

    assert_eq!(stamped, sender);
    assert_eq!(ctx.actor_path(stamped), expected);

    drop_ref(&registry, sender);

    assert_eq!(ctx.sender(), Some(sender), "a departed sender still holds its route record");
    assert_eq!(ctx.actor_path(stamped), expected);
}

/// A sink that discharges each dispatch, then forwards it.
fn forwarding(tx: mpsc::Sender<OwnedDispatch>) -> Arc<dyn InboxHandler> {
    Arc::new(move |dispatch: OwnedDispatch| {
        dispatch.discharge();
        let _ = tx.send(dispatch);
    })
}

/// Catches `stage_registry_batch_from` arming its completion from the staging
/// ctx instead of the owed value: that would take a second settlement hold on
/// the caller's chain and route the terminal reply to whichever target the
/// staging ctx names. The batch's completion must own the one hold the
/// `Held` took and answer the caller the `Held` captured, even from a later
/// ctx whose reply target is someone else.
#[test]
fn a_batch_staged_from_a_held_reply_answers_the_held_caller() {
    let (registry, mailer) = bare_substrate();
    let counter = Arc::clone(mailer.trace_handle().settlement_counter());
    let (caller_tx, caller_rx) = mpsc::channel::<OwnedDispatch>();
    let caller = registry.register_inbox(&boot_authority(), "test.native.batch_from.caller", forwarding(caller_tx));
    let (other_tx, other_rx) = mpsc::channel::<OwnedDispatch>();
    let other = registry.register_inbox(&boot_authority(), "test.native.batch_from.other", forwarding(other_tx));
    let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
    let host_name = "test.native.batch_from.host";
    let host = registry.register_inbox(&boot_authority(), host_name, forwarding(wake_tx));

    // The registry has no owner attached, so the flushed batch completes at
    // once (`OwnerClosed`) and wakes the host: the reply routing under test
    // does not depend on the batch's outcome.
    let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
    let pool = Pool::start(PoolConfig { workers: 1, ..PoolConfig::default() }, Arc::clone(&aborter));
    let spawner = Arc::new(crate::Spawner::new(
        Arc::clone(&registry),
        Arc::new(ActorRegistry::new()),
        Arc::clone(&mailer),
        Arc::clone(&aborter),
        pool.wake_sink(),
        RingCapacities::default(),
    ));
    let binding = Arc::new(NativeBinding::new(
        Arc::clone(&mailer),
        host,
        host.0,
        ErasedActorPath::new(host_name).expect("the host name is a canonical path"),
        aborter,
        Some(spawner),
    ));
    let root = MailId::new(MailboxId(0xC3), 1);

    let staged = {
        let mut ctx =
            NativeCtx::new(&binding, Source::with_correlation(SourceAddr::Component(caller), 77), None, Some(root));
        let (pending, held) = ctx.hold::<TestReply>();
        pending.__defuse();
        let held_id = held.dispatch_id();
        let staged = ctx.stage_registry_batch_from(held, RegistryBatch::register_kinds(Vec::new()), ());

        assert_eq!(binding.dispatch_state_of(held_id), None, "staging moved the held entry into the batch's");
        assert_eq!(counter.held_open(root), 1, "the batch owns the one hold the Held took, not a second");
        staged
    };

    let wake = wake_rx.recv_timeout(Duration::from_secs(2)).expect("the flushed batch completes and wakes the host");
    let wake = TaskCompletionWake::decode_from_bytes(wake.payload.bytes()).expect("the completion wake decodes");
    assert_eq!(DispatchId(wake.dispatch_id), staged);
    assert_eq!(counter.held_open(root), 1, "the completed batch still owes the caller");

    {
        let mut later =
            NativeCtx::new(&binding, Source::with_correlation(SourceAddr::Component(other), 99), None, None);
        let done = binding
            .dispatch_take::<RegistryBatchResult, ()>(staged)
            .expect("the batch's completion waits in the ledger");
        done.resolve_value(&mut later, &TestReply { value: 5 });
    }

    let reply = caller_rx.recv_timeout(Duration::from_secs(2)).expect("the answer reaches the held caller");
    assert_eq!(reply.sender.correlation_id, 77, "the held correlation is echoed, not the later ctx's");
    assert_eq!(TestReply::decode_from_bytes(reply.payload.bytes()), Some(TestReply { value: 5 }));
    assert!(other_rx.try_recv().is_err(), "the later ctx's reply target hears nothing");
    assert_eq!(counter.held_open(root), 0, "resolving releases the one hold");
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}
