//! Shared dispatcher loop for native actors (issue 672).
//!
//! `dispatch_loop_run<A>` is the body every native-actor dispatcher
//! thread runs. Singleton boots through `chassis::builder` and
//! instanced spawns through `actor::native::spawn` both call into
//! this — eliminating the historical divergence where instanced
//! actors lacked the `local::with_stamped` wrapping the singleton
//! path had.
//!
//! ## Lifecycle
//!
//! 1. **Outer loop.** Polls `binding.should_shutdown()` (set by
//!    `NativeCtx::shutdown`), then `binding.recv_blocking()`. Either
//!    signal exits the loop.
//! 2. **Per-envelope dispatch.** Each envelope runs inside
//!    `local::with_stamped(slots, ...)` so the per-actor `ActorSlots`
//!    are visible to `Local<T>` lookups — including the per-actor
//!    [`ActorLogRing`] the `ActorAwareLayer` pushes into and the
//!    framework-built-in `aether.log.tail` handler reads from
//!    (ADR-0081 §1). Before the user dispatch runs, the framework
//!    intercepts `aether.log.tail` envelopes and replies from the
//!    actor's ring directly. Two-step typed → fallback dispatch
//!    follows for everything else.
//! 3. **Drain after shutdown.** Any envelope already in the inbox
//!    when the shutdown signal fired is processed synchronously
//!    before `unwire` runs (matches the existing singleton
//!    semantics).
//! 4. **`unwire`.** Last-chance hook with `ReplyTo::NONE`. Wrapped
//!    in the same `with_stamped` so any final tracing or `Local<T>`
//!    access works.
//! 5. **Registry close + monitor fan-out.** `actor_registry.close_actor(id)`
//!    drains `monitors_of[id]`, prunes `monitoring[id]` from each
//!    target, marks the slot Dead. Returned watchers receive a
//!    `MonitorNotice` mail through the supplied `Mailer`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aether_actor::Local;
use aether_actor::MailCtx;
use aether_actor::local::ActorSlots;
use aether_actor::log::ActorLogRing;
use aether_data::Kind;
use aether_kinds::{LogTail, LogTailResult};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::{NativeActor, NativeDispatch};
use crate::actor::registry::ActorRegistry;
use crate::mail::mailer::Mailer;
use crate::mail::{KindId, Mail, MailId, MailboxId, ReplyTo};
use aether_actor::local;
use std::thread;

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

/// ADR-0081 framework-built-in dispatch arm for `aether.log.tail`.
/// Returns `true` when the envelope's kind matches `LogTail` (the
/// reply is sent from inside this fn); the dispatcher then skips
/// the user's typed/fallback dispatch for this envelope. Reads the
/// caller's `ActorLogRing` via the currently-stamped `ActorSlots` —
/// the framework arm runs *inside* the same `local::with_stamped`
/// the dispatch loop already opens, so `try_with` resolves to the
/// receiving actor's ring without any extra plumbing.
///
/// Returning `Err` is reserved for a future "ring not materialised"
/// failure mode; today the per-actor ring is initialised in the
/// dispatcher's `local::with_stamped` setup unconditionally, so a
/// missing ring would be a substrate-level invariant violation.
pub fn dispatch_log_tail_if_matching(ctx: &mut NativeCtx<'_>, env: &Envelope) -> bool {
    if env.kind.0 != <LogTail as Kind>::ID.0 {
        return false;
    }
    let Some(request) = <LogTail as Kind>::decode_from_bytes(&env.payload) else {
        ctx.reply(&LogTailResult::Err {
            error: "aether.log.tail: payload failed to decode".to_owned(),
        });
        return true;
    };
    let reply =
        ActorLogRing::try_with(|ring| ring.tail(&request)).unwrap_or_else(|| LogTailResult::Err {
            error: "aether.log.tail: actor has no stamped slots".to_owned(),
        });
    ctx.reply(&reply);
    true
}

/// Run one actor's dispatcher loop on the calling thread. Returns
/// when the binding signals shutdown (self-shutdown flag set or
/// inbox sender disconnected). See module doc-comment for the full
/// lifecycle.
///
/// `pending` is decremented after every dispatched envelope when
/// `Some`. Singletons now always pass `None` — ADR-0082 retired the
/// frame-bound drain barrier that was the singleton consumer.
/// Instanced actors pass their per-actor counter, which
/// `Spawner::shutdown_instanced` reads to coordinate teardown (issue
/// 685).
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
        let thread_name = thread::current().name().map(str::to_owned);
        binding
            .mailer()
            .record_received(inbound_mail_id, env.root, thread_name);
        local::with_stamped(slots, || {
            let mut ctx = NativeCtx::new(binding, env.sender, env.mail_id, env.root);
            // ADR-0081: framework intercepts `aether.log.tail` before
            // the actor's typed/fallback dispatch. The actor never
            // sees the envelope; the framework reads its ring and
            // replies inline.
            if !dispatch_log_tail_if_matching(&mut ctx, &env) {
                typed_then_fallback_or_warn::<A>(actor, &mut ctx, &env);
            }
        });
        binding.mailer().record_finished(inbound_mail_id, env.root);
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
        let thread_name = thread::current().name().map(str::to_owned);
        binding
            .mailer()
            .record_received(inbound_mail_id, env.root, thread_name);
        local::with_stamped(slots, || {
            let mut ctx = NativeCtx::new(binding, env.sender, env.mail_id, env.root);
            if !dispatch_log_tail_if_matching(&mut ctx, &env) {
                let _ = actor.__aether_dispatch_envelope(&mut ctx, env.kind, &env.payload);
            }
        });
        binding.mailer().record_finished(inbound_mail_id, env.root);
        if let Some(p) = pending {
            p.fetch_sub(1, Ordering::AcqRel);
        }
    }

    // Phase 3: last-chance close hook. ReplyTo is None — no inbound
    // envelope produced this call.
    local::with_stamped(slots, || {
        let mut close_ctx = NativeCtx::new(binding, ReplyTo::NONE, MailId::NONE, MailId::NONE);
        actor.unwire(&mut close_ctx);
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
        let payload = <aether_kinds::MonitorNotice as Kind>::encode_into_bytes(&notice);
        let kind = KindId(<aether_kinds::MonitorNotice as Kind>::ID.0);
        for watcher in watchers {
            mailer.push(Mail::new(watcher, kind, payload.clone(), 1));
        }
    }
}
