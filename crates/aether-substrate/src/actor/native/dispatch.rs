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
use aether_actor::cost::CostCells;
use aether_actor::local::ActorSlots;
use aether_actor::log::ActorLogRing;
use aether_actor::trace_ring::ActorTraceRing;
use aether_data::Kind;
use aether_kinds::trace::{Nanos, TraceEvent, TraceTail, TraceTailResult};
use aether_kinds::{CostTail, CostTailResult, LogTail, LogTailResult};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::{NativeActor, NativeDispatch};
use crate::actor::registry::ActorRegistry;
use crate::mail::mailer::Mailer;
use crate::mail::{KindId, Mail, MailId, MailboxId, ReplyTo};
use crate::runtime::thread_name;
use aether_actor::local;

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
        .__aether_dispatch_envelope(ctx, env.kind, env.payload.bytes())
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
    let Some(request) = <LogTail as Kind>::decode_from_bytes(env.payload.bytes()) else {
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

/// ADR-0086 Phase 3 framework-built-in dispatch arm for
/// `aether.trace.tail` — the trace-side sibling of
/// [`dispatch_log_tail_if_matching`]. Reads the receiving actor's
/// [`ActorTraceRing`] via the currently-stamped `ActorSlots` and
/// replies inline; the dispatcher then skips the user's typed/fallback
/// dispatch for this envelope. The trace-tree coordinator fans this out
/// across live actors and stitches the per-ring slices.
pub fn dispatch_trace_tail_if_matching(ctx: &mut NativeCtx<'_>, env: &Envelope) -> bool {
    if env.kind.0 != <TraceTail as Kind>::ID.0 {
        return false;
    }
    let Some(request) = <TraceTail as Kind>::decode_from_bytes(env.payload.bytes()) else {
        ctx.reply(&TraceTailResult::Err {
            error: "aether.trace.tail: payload failed to decode".to_owned(),
        });
        return true;
    };
    let reply = ActorTraceRing::try_with(|ring| ring.tail(&request)).unwrap_or_else(|| {
        TraceTailResult::Err {
            error: "aether.trace.tail: actor has no stamped slots".to_owned(),
        }
    });
    ctx.reply(&reply);
    true
}

/// iamacoffeepot/aether#1128 framework-built-in dispatch arm for
/// `aether.cost.tail` — the cost-side sibling of
/// [`dispatch_log_tail_if_matching`] / [`dispatch_trace_tail_if_matching`].
/// Reads the receiving actor's per-handler execution-cost EWMA from the
/// global [`CostTable`](crate::mail::cost::CostTable) (filtered to this
/// actor's mailbox) and replies inline; the dispatcher then skips the
/// user's typed/fallback dispatch for this envelope. Cold path — read
/// lock fine (the per-dispatch fold runs lock-free through the per-actor
/// `CostCells` cache, never here).
pub fn dispatch_cost_tail_if_matching(
    binding: &NativeBinding,
    ctx: &mut NativeCtx<'_>,
    env: &Envelope,
) -> bool {
    if env.kind.0 != <CostTail as Kind>::ID.0 {
        return false;
    }
    let Some(request) = <CostTail as Kind>::decode_from_bytes(env.payload.bytes()) else {
        ctx.reply(&CostTailResult::Err {
            error: "aether.cost.tail: payload failed to decode".to_owned(),
        });
        return true;
    };
    // Read the global cost table filtered to this actor's mailbox (cold
    // path, read lock fine) so the dump surfaces the load-time
    // neutral-seed rows even before any dispatch has folded a sample.
    let reply = binding
        .mailer()
        .cost_table()
        .tail(binding.self_mailbox(), &request);
    ctx.reply(&reply);
    true
}

/// iamacoffeepot/aether#1128 dark-instrumentation fold. Folds one
/// handler-execution sample — `now − t_received`, the existing
/// `(Finished.t − Received.t)` trace bracket with no new clock read on
/// the fast path — into the per-handler [`aether_actor::cost::CostCell`]
/// EWMA. Runs inside the dispatch `local::with_stamped` block on the
/// actor's own thread, so it reaches its cell through the lock-free
/// per-actor [`CostCells`] cache; a kind not in the cache (the framework
/// arms `log.tail` / `trace.tail` / `cost.tail`, or a fallback dispatch)
/// is skipped — that's the known-handler filter.
///
/// The per-actor cache is lazy-seeded from the global
/// [`CostTable`](crate::mail::cost::CostTable) on first dispatch: the
/// cap-registry seed hook runs cross-thread for the wasm-load path and
/// can't stamp the actor's `Local<T>` directly, so the actual stamp into
/// the cache happens here, once, on the actor's own thread — pulling the
/// *same* `Arc<CostCell>`s the global table holds (the shared-index
/// invariant). After that the lookup is a lock-free linear scan over a
/// tiny `Vec`.
///
/// **No scheduling change** — this only writes the cell.
pub fn fold_handler_cost(
    binding: &NativeBinding,
    kind: KindId,
    t_received: Nanos,
    finished: Nanos,
) {
    // Sample = handler execution time. `finished >= t_received` always
    // (same monotonic clock, finished stamped after received); guard the
    // subtraction anyway so a clock anomaly can't underflow.
    let sample = finished.0.saturating_sub(t_received.0);
    CostCells::try_with_mut(|cells| {
        if !cells.is_seeded() {
            // First dispatch on this actor's thread: pull the shared
            // cells the load-time seed planted in the global table.
            // Empty stays empty (no declared handlers / not yet seeded
            // globally) — a later dispatch retries the pull cheaply.
            let pulled = binding
                .mailer()
                .cost_table()
                .cells_for(binding.self_mailbox());
            if !pulled.is_empty() {
                cells.seed(pulled);
            }
        }
        if let Some(cell) = cells.get(kind) {
            cell.fold(sample);
        }
    });
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
        // Issue 734 / ADR-0088 §7: stamp the dispatching thread's
        // name-hashed `ThreadId` (cached per thread, zero per-hop alloc).
        let thread_id = thread_name::current_thread_id();
        local::with_stamped(slots, || {
            // ADR-0086 Phase 3: `Received` / `Finished` land in this
            // (recipient) actor's trace ring — only inside this
            // `with_stamped` is its `ActorSlots` stamped.
            let th = binding.mailer().trace_handle();
            // iamacoffeepot/aether#1128: capture the `Received` instant
            // so the cost fold below reuses the existing trace bracket —
            // no new timestamp on the hot path.
            let t_received = th.now_nanos();
            th.push_trace_ring(
                env.root,
                TraceEvent::Received {
                    mail_id: inbound_mail_id,
                    t: t_received,
                    // iamacoffeepot/aether#1134: the producer-stamped
                    // deposit instant + scheduler backlog, splitting the
                    // hop into send→enqueue + queue residence.
                    t_enqueue: env.t_enqueue,
                    enqueue_depth: env.enqueue_depth,
                    thread_id,
                },
            );
            let mut ctx = NativeCtx::new(binding, env.sender, env.mail_id, env.root);
            // ADR-0081 / ADR-0086 / iamacoffeepot/aether#1128: the
            // framework intercepts `aether.log.tail`,
            // `aether.trace.tail`, and `aether.cost.tail` before the
            // actor's typed/fallback dispatch. The actor never sees
            // these; the framework reads its rings / cost table and
            // replies inline.
            if !dispatch_log_tail_if_matching(&mut ctx, &env)
                && !dispatch_trace_tail_if_matching(&mut ctx, &env)
                && !dispatch_cost_tail_if_matching(binding, &mut ctx, &env)
            {
                typed_then_fallback_or_warn::<A>(actor, &mut ctx, &env);
            }
            // iamacoffeepot/aether#1150: drop `ctx` now to flush the
            // handler's buffered sends (stamping each child `Sent` at
            // flush-begin) before `Finished`, so a child's `t_sent`
            // precedes its parent's `t_finished` — the causal order the
            // trace walk expects. Otherwise the flush rides `ctx`'s
            // scope-end `Drop`, landing after this push.
            drop(ctx);
            let t_finished = th.now_nanos();
            th.push_trace_ring(
                env.root,
                TraceEvent::Finished {
                    mail_id: inbound_mail_id,
                    t: t_finished,
                },
            );
            // iamacoffeepot/aether#1128: fold this handler's execution
            // time `(t_finished − t_received)` into its per-handler EWMA.
            // Lock-free through the per-actor `CostCells` cache; a
            // framework arm / fallback kind (not in the cache) is
            // skipped. Measure-only — no scheduling change.
            fold_handler_cost(binding, env.kind, t_received, t_finished);
        });
        // ADR-0080 §2 settlement hook, outside `with_stamped` so the
        // `fire_settled` notification runs unstamped (it may resolve mail
        // subscribers inline).
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
        let thread_id = thread_name::current_thread_id();
        local::with_stamped(slots, || {
            let th = binding.mailer().trace_handle();
            let t_received = th.now_nanos();
            th.push_trace_ring(
                env.root,
                TraceEvent::Received {
                    mail_id: inbound_mail_id,
                    t: t_received,
                    // iamacoffeepot/aether#1134: the producer-stamped
                    // deposit instant + scheduler backlog, splitting the
                    // hop into send→enqueue + queue residence.
                    t_enqueue: env.t_enqueue,
                    enqueue_depth: env.enqueue_depth,
                    thread_id,
                },
            );
            let mut ctx = NativeCtx::new(binding, env.sender, env.mail_id, env.root);
            if !dispatch_log_tail_if_matching(&mut ctx, &env)
                && !dispatch_trace_tail_if_matching(&mut ctx, &env)
                && !dispatch_cost_tail_if_matching(binding, &mut ctx, &env)
            {
                let _ = actor.__aether_dispatch_envelope(&mut ctx, env.kind, env.payload.bytes());
            }
            // iamacoffeepot/aether#1150: flush before `Finished` so a
            // child `Sent` (stamped at flush-begin on `ctx` drop) precedes
            // its parent's `Finished`. See the main-loop arm above.
            drop(ctx);
            let t_finished = th.now_nanos();
            th.push_trace_ring(
                env.root,
                TraceEvent::Finished {
                    mail_id: inbound_mail_id,
                    t: t_finished,
                },
            );
            // iamacoffeepot/aether#1128: fold execution cost (see the
            // main-loop arm above).
            fold_handler_cost(binding, env.kind, t_received, t_finished);
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic on failure is the assertion"
)]
mod cost_tests {
    use super::*;
    use crate::actor::native::binding::NativeBinding;
    use crate::test_util::fresh_substrate;
    use aether_actor::local::{ActorSlots, with_stamped};
    use aether_kinds::{CostTail, CostTailResult};

    /// iamacoffeepot/aether#1128 step 4: folding a known handler's
    /// execution time moves its cell (through the lazy per-actor cache
    /// pull from the seeded global table), and the global `cost.tail`
    /// dump surfaces the moved row.
    #[test]
    fn fold_moves_seeded_handler_cell() {
        let (_registry, mailer) = fresh_substrate();
        let self_mbx = MailboxId(0x1128);
        let handled = KindId(10);
        // Load-time seed: a neutral cell for the actor's one handler.
        mailer.cost_table().seed(self_mbx, &[handled]);

        let binding = NativeBinding::new_for_test(Arc::clone(&mailer), self_mbx);
        let slots = ActorSlots::new();
        // Fold runs inside `with_stamped` on the actor's own thread, as
        // the dispatch loop does — the lazy pull stamps the cache here.
        with_stamped(&slots, || {
            fold_handler_cost(&binding, handled, Nanos(1_000), Nanos(6_000));
        });

        let CostTailResult::Ok { rows } =
            mailer.cost_table().tail(self_mbx, &CostTail { kind: None })
        else {
            panic!("expected Ok");
        };
        let row = rows
            .iter()
            .find(|r| r.kind_id == handled)
            .expect("handled kind's row present");
        assert_eq!(row.samples, 1, "one sample folded");
        assert_eq!(row.mean_nanos, 5_000, "(finished − received) = 5000ns");
    }

    /// iamacoffeepot/aether#1128 step 4: a kind NOT in the actor's
    /// seeded handler set (a framework arm like `log.tail`, or fallback
    /// dispatch) leaves no cell — the fold's known-handler filter.
    #[test]
    fn fold_skips_unseeded_kind() {
        let (_registry, mailer) = fresh_substrate();
        let self_mbx = MailboxId(0x1128);
        let handled = KindId(10);
        mailer.cost_table().seed(self_mbx, &[handled]);

        let binding = NativeBinding::new_for_test(Arc::clone(&mailer), self_mbx);
        let slots = ActorSlots::new();
        // Fold a kind the actor never declared (a framework / fallback
        // kind) — it must not create a row.
        let stranger = KindId(<LogTail as Kind>::ID.0);
        with_stamped(&slots, || {
            fold_handler_cost(&binding, stranger, Nanos(0), Nanos(9_999));
        });

        let CostTailResult::Ok { rows } =
            mailer.cost_table().tail(self_mbx, &CostTail { kind: None })
        else {
            panic!("expected Ok");
        };
        assert!(
            rows.iter().all(|r| r.kind_id != stranger),
            "an unseeded kind folds into no cell",
        );
        // The seeded handler stays at its neutral seed (samples = 0).
        let seeded_row = rows.iter().find(|r| r.kind_id == handled).unwrap();
        assert_eq!(seeded_row.samples, 0);
    }

    /// iamacoffeepot/aether#1128 step 6: the `cost.tail` framework arm
    /// returns the actor's rows (filtered to `CostTail::kind` when set)
    /// from the global table. Exercised directly through the table the
    /// arm reads — `dispatch_cost_tail_if_matching` is a thin wrapper
    /// over `cost_table().tail(self_mailbox, &request)`.
    #[test]
    fn cost_tail_arm_reports_seeded_rows() {
        let (_registry, mailer) = fresh_substrate();
        let self_mbx = MailboxId(0x1128);
        mailer
            .cost_table()
            .seed(self_mbx, &[KindId(10), KindId(20)]);

        let CostTailResult::Ok { rows } = mailer.cost_table().tail(
            self_mbx,
            &CostTail {
                kind: Some(KindId(20)),
            },
        ) else {
            panic!("expected Ok");
        };
        assert_eq!(rows.len(), 1, "kind filter narrows the dump");
        assert_eq!(rows[0].kind_id, KindId(20));
        assert_eq!(
            rows[0].samples, 0,
            "neutral seed surfaces before any dispatch"
        );
    }
}
