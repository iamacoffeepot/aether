//! Shared dispatcher loop for native actors (issue 672).
//!
//! `dispatch_loop_run<A>` is the body every native-actor dispatcher
//! thread runs. Singleton boots through `chassis::builder` and
//! instanced spawns through `actor::native::spawn` both call into
//! this — eliminating the historical divergence where instanced
//! actors lacked the `local::with_stamped` + `log_install::with_actor_dispatch`
//! wrapping the singleton path had.
//!
//! ## Lifecycle
//!
//! 1. **Outer loop.** Polls `binding.should_shutdown()` (set by
//!    `NativeCtx::shutdown`), then `binding.recv_blocking()`. Either
//!    signal exits the loop.
//! 2. **Per-envelope dispatch.** Each envelope runs inside
//!    `local::with_stamped(slots, ...)` so the per-actor `ActorSlots`
//!    are visible to `Local<T>` lookups, and inside
//!    `log_install::with_actor_dispatch(binding, ...)` so the actor-
//!    aware `tracing` layer attributes events with the actor's
//!    `MailboxId` and the priority-flush + post-handler drain ship a
//!    `LogBatch` to `LogCapability`. Two-step typed → fallback dispatch.
//! 3. **Drain after shutdown.** Any envelope already in the inbox
//!    when the shutdown signal fired is processed synchronously
//!    before `unwire` runs (matches the existing singleton
//!    semantics).
//! 4. **`unwire`.** Last-chance hook with `ReplyTo::NONE`. Wrapped
//!    in the same `with_stamped` + `with_actor_dispatch` so any
//!    final tracing or `Local<T>` access works.
//! 5. **Registry close + monitor fan-out.** `actor_registry.close_actor(id)`
//!    drains `monitors_of[id]`, prunes `monitoring[id]` from each
//!    target, marks the slot Dead. Returned watchers receive a
//!    `MonitorNotice` mail through the supplied `Mailer`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aether_actor::local::ActorSlots;

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::{NativeActor, NativeDispatch};
use crate::actor::registry::ActorRegistry;
use crate::mail::mailer::Mailer;
use crate::mail::{KindId, Mail, MailId, MailboxId, ReplyTo};

/// Try the typed `#[handler]` dispatch; if no typed arm matches and
/// the actor's `#[fallback]` also returns `false`, warn that the kind
/// fell through. Shared by `dispatch_loop_run`'s main loop and
/// `DispatcherSlot::run_cycle`'s pool path.
///
/// Issue 576 framing: catch-all caps that own a `#[fallback]` return
/// `true` after their fallback runs, which suppresses the warn.
/// Strict receivers keep the default (`false`) so the miss surfaces.
pub fn typed_then_fallback_or_warn<A>(actor: &mut Box<A>, ctx: &mut NativeCtx<'_>, env: &Envelope)
where
    A: NativeActor + NativeDispatch,
{
    if actor
        .__aether_dispatch_envelope(ctx, env.kind, &env.payload)
        .is_none()
        && !actor.__aether_dispatch_fallback(ctx, env)
    {
        tracing::warn!(
            target: "aether_substrate::dispatch",
            actor = A::NAMESPACE,
            kind = env.kind_name.as_str(),
            "actor dispatch missed: kind not handled or decode failed"
        );
    }
}

/// Run one actor's dispatcher loop on the calling thread. Returns
/// when the binding signals shutdown (self-shutdown flag set or
/// inbox sender disconnected). See module doc-comment for the full
/// lifecycle.
///
/// `pending` is decremented after every dispatched envelope when
/// `Some` — singletons pass it for `FRAME_BARRIER` caps (the chassis
/// frame-loop drain barrier reads it); instanced actors pass their
/// per-actor counter (no live consumer post-PR-4: `wait_instanced_quiesce`
/// retired in favour of ADR-0080 settlement gating, but the counter
/// stays plumbed for the trampoline's `tx.send` accounting).
pub fn dispatch_loop_run<A>(
    binding: &Arc<NativeBinding>,
    actor: &mut Box<A>,
    slots: &ActorSlots,
    pending: Option<&Arc<AtomicU64>>,
    actor_registry: &Arc<ActorRegistry>,
    mailer: &Arc<Mailer>,
    self_id: MailboxId,
) where
    A: NativeActor + NativeDispatch,
{
    // Phase 1: main dispatch loop.
    loop {
        if binding.should_shutdown() {
            break;
        }
        let Some(env) = binding.recv_blocking() else {
            break;
        };
        let inbound_mail_id = env.mail_id;
        // ADR-0080 §2 producer hook: `Received` at handler entry.
        // Issue 734: capture the OS thread name so the trace renderer
        // can stamp per-thread tids + emit thread_name M events. With
        // the default `Pooled` scheduler (issue 635) this surfaces as
        // `aether-worker-N` shared across actors; `Thread`-scheduled
        // actors get per-actor names.
        let thread_name = std::thread::current().name().map(str::to_owned);
        crate::runtime::trace::record_received(inbound_mail_id, thread_name);
        aether_actor::local::with_stamped(slots, || {
            crate::runtime::log_install::with_actor_dispatch(
                &**binding as &dyn crate::runtime::log_install::MailDispatch,
                || {
                    let mut ctx = NativeCtx::new(binding, env.sender, env.mail_id, env.root);
                    typed_then_fallback_or_warn::<A>(actor, &mut ctx, &env);
                    aether_actor::log::drain_buffer();
                },
            );
        });
        // ADR-0080 §2 producer hook: `Finished` at handler exit. PR 2
        // does not bracket the panic-unwind path; if a handler panics
        // mid-dispatch the actor's process-level panic hook brings
        // the substrate down anyway, so a missing `Finished` is
        // moot. A future PR may add `catch_unwind` here for graceful
        // settlement-on-panic.
        crate::runtime::trace::record_finished(inbound_mail_id);
        if let Some(p) = pending {
            p.fetch_sub(1, Ordering::AcqRel);
        }
    }

    // Phase 2: drain remaining inbox synchronously. The shutdown
    // flag / disconnect raced against any in-flight mail the sink
    // handler already pushed; the actor sees it before `unwire`
    // runs so a "please close" handler that flushes state observes
    // the full inbox.
    while let Some(env) = binding.try_recv() {
        let inbound_mail_id = env.mail_id;
        let thread_name = std::thread::current().name().map(str::to_owned);
        crate::runtime::trace::record_received(inbound_mail_id, thread_name);
        aether_actor::local::with_stamped(slots, || {
            crate::runtime::log_install::with_actor_dispatch(
                &**binding as &dyn crate::runtime::log_install::MailDispatch,
                || {
                    let mut ctx = NativeCtx::new(binding, env.sender, env.mail_id, env.root);
                    let _ = actor.__aether_dispatch_envelope(&mut ctx, env.kind, &env.payload);
                    aether_actor::log::drain_buffer();
                },
            );
        });
        crate::runtime::trace::record_finished(inbound_mail_id);
        if let Some(p) = pending {
            p.fetch_sub(1, Ordering::AcqRel);
        }
    }

    // Phase 3: last-chance close hook. ReplyTo is None — no inbound
    // envelope produced this call.
    aether_actor::local::with_stamped(slots, || {
        crate::runtime::log_install::with_actor_dispatch(
            &**binding as &dyn crate::runtime::log_install::MailDispatch,
            || {
                let mut close_ctx =
                    NativeCtx::new(binding, ReplyTo::NONE, MailId::NONE, MailId::NONE);
                actor.unwire(&mut close_ctx);
                aether_actor::log::drain_buffer();
            },
        );
    });

    // Phase 4: close in the registry — drains `monitors_of[id]` for
    // fan-out, prunes `monitoring[id]` from each watched target,
    // marks Dead + tombstones the id. Singletons today don't sit in
    // `actors` as `Live`, so the slot transition is purely sentinel;
    // the reverse-prune is the load-bearing step. Instanced actors
    // do sit Live and transition Live → Dead here.
    let watchers = actor_registry.close_actor(self_id);
    if !watchers.is_empty() {
        let notice = aether_kinds::MonitorNotice { target: self_id };
        let payload =
            <aether_kinds::MonitorNotice as aether_data::Kind>::encode_into_bytes(&notice);
        let kind = KindId(<aether_kinds::MonitorNotice as aether_data::Kind>::ID.0);
        for watcher in watchers {
            mailer.push(Mail::new(watcher, kind, payload.clone(), 1));
        }
    }
}
