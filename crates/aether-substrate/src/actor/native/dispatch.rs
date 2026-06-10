//! Shared dispatch helpers for the pooled `DispatcherSlot` path
//! (issue 672; per-thread `Dedicated` dispatch removed in issue 1187).
//!
//! Every native actor — singleton chassis cap or instanced spawn —
//! drains cooperatively on the chassis worker pool via
//! [`crate::actor::native::dispatcher_slot::DispatcherSlot::run_cycle`].
//! That loop owns the lifecycle (recv → per-envelope `local::with_stamped`
//! dispatch → drain-on-shutdown → `unwire` → registry close + monitor
//! fan-out); this module holds the per-envelope helpers it calls:
//!
//! - [`typed_then_fallback_or_warn`] — typed `#[handler]` → `#[fallback]`
//!   → warn-on-miss, the user-dispatch step.
//! - [`dispatch_log_tail_if_matching`] / [`dispatch_trace_tail_if_matching`]
//!   / [`dispatch_cost_tail_if_matching`] — the framework-built-in arms
//!   for `aether.{log,trace,cost}.tail`, which read the receiving
//!   actor's stamped per-actor rings / cost table and reply inline
//!   before the user dispatch runs (ADR-0081 §1 / ADR-0086 Phase 3 /
//!   iamacoffeepot/aether#1128).
//! - [`fold_handler_cost`] — folds one handler-execution sample into
//!   the per-handler EWMA (iamacoffeepot/aether#1128, measure-only).

use aether_actor::Local;
use aether_actor::OutboundReply;
use aether_actor::cost::CostCells;
use aether_actor::log::ActorLogRing;
use aether_actor::trace_ring::ActorTraceRing;
use aether_data::Kind;
use aether_kinds::trace::{Nanos, TraceTail, TraceTailResult};
use aether_kinds::{CostTail, CostTailResult, LogTail, LogTailResult};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::{NativeActor, NativeDispatch};
use crate::mail::mailer::Mailer;
use crate::mail::{KindId, MailboxId, Source};

/// Try the typed `#[handler]` dispatch; if no typed arm matches and
/// the actor's `#[fallback]` also returns `false`, warn that the kind
/// fell through. Called from `DispatcherSlot::run_cycle`'s pool path.
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

/// iamacoffeepot/aether#1272: `NativeCtx`-free variant of
/// `dispatch_log_tail_if_matching` for driver-as-actor capabilities
/// that own their inbox drain inline (today only the desktop window
/// driver). Reads the receiving actor's `ActorLogRing` through the
/// currently-stamped `ActorSlots` and routes the reply via the supplied
/// `Mailer::send_reply` — the same path `NativeCtx::reply` goes through.
///
/// Caller invariant: this must be called inside a
/// `local::with_stamped(&actor_slots, …)` block so `ActorLogRing::try_with`
/// resolves to the driver's per-actor ring.
pub fn dispatch_log_tail_if_matching_free(
    mailer: &Mailer,
    reply_to: Source,
    env: &Envelope,
) -> bool {
    if env.kind.0 != <LogTail as Kind>::ID.0 {
        return false;
    }
    let Some(request) = <LogTail as Kind>::decode_from_bytes(env.payload.bytes()) else {
        mailer.send_reply(
            reply_to,
            &LogTailResult::Err {
                error: "aether.log.tail: payload failed to decode".to_owned(),
            },
        );
        return true;
    };
    let reply =
        ActorLogRing::try_with(|ring| ring.tail(&request)).unwrap_or_else(|| LogTailResult::Err {
            error: "aether.log.tail: actor has no stamped slots".to_owned(),
        });
    mailer.send_reply(reply_to, &reply);
    true
}

/// iamacoffeepot/aether#1272: `NativeCtx`-free counterpart of
/// `dispatch_trace_tail_if_matching`. See
/// [`dispatch_log_tail_if_matching_free`] for the caller invariant.
pub fn dispatch_trace_tail_if_matching_free(
    mailer: &Mailer,
    reply_to: Source,
    env: &Envelope,
) -> bool {
    if env.kind.0 != <TraceTail as Kind>::ID.0 {
        return false;
    }
    let Some(request) = <TraceTail as Kind>::decode_from_bytes(env.payload.bytes()) else {
        mailer.send_reply(
            reply_to,
            &TraceTailResult::Err {
                error: "aether.trace.tail: payload failed to decode".to_owned(),
            },
        );
        return true;
    };
    let reply = ActorTraceRing::try_with(|ring| ring.tail(&request)).unwrap_or_else(|| {
        TraceTailResult::Err {
            error: "aether.trace.tail: actor has no stamped slots".to_owned(),
        }
    });
    mailer.send_reply(reply_to, &reply);
    true
}

/// iamacoffeepot/aether#1272: `NativeCtx`-free counterpart of
/// `dispatch_cost_tail_if_matching`. The cost table doesn't depend on
/// stamped `ActorSlots` — it's read directly off the mailer — so the
/// `self_mailbox` rides along explicitly (the standard variant pulls it
/// from `binding.self_mailbox()`).
pub fn dispatch_cost_tail_if_matching_free(
    mailer: &Mailer,
    reply_to: Source,
    self_mailbox: MailboxId,
    env: &Envelope,
) -> bool {
    if env.kind.0 != <CostTail as Kind>::ID.0 {
        return false;
    }
    let Some(request) = <CostTail as Kind>::decode_from_bytes(env.payload.bytes()) else {
        mailer.send_reply(
            reply_to,
            &CostTailResult::Err {
                error: "aether.cost.tail: payload failed to decode".to_owned(),
            },
        );
        return true;
    };
    let reply = mailer.cost_table().tail(self_mailbox, &request);
    mailer.send_reply(reply_to, &reply);
    true
}

/// iamacoffeepot/aether#1128 dark-instrumentation fold. Folds one
/// handler-execution sample — `finished − t_received`, the existing
/// `(Finished.t − Received.t)` trace bracket with no new clock read on
/// the fast path — into the per-handler [`aether_actor::cost::CostCell`]
/// EWMA. Runs inside the dispatch `local::with_stamped` block, so it
/// reaches its cell through the lock-free per-actor [`CostCells`] cache.
/// A kind not in the cache (the framework arms `log.tail` / `trace.tail`
/// / `cost.tail`, or a fallback dispatch) is skipped — the known-handler
/// filter.
///
/// The cache is seeded once at actor construction — `WasmTrampoline::init`
/// for components, the native-cap boot wrap for caps — both inside the
/// same `with_stamped(&slots, …)` the spawn path opens around `init`
/// (the stamp binds to the actor's `ActorSlots`, not to a thread, so the
/// seed runs wherever construction does). Every declared handler's cell
/// is therefore present before the first dispatch: no lazy first-dispatch
/// pull, no per-fold lock — just a linear scan over a tiny `Vec`.
///
/// **No scheduling change** — this only writes the cell.
pub fn fold_handler_cost(kind: KindId, t_received: Nanos, finished: Nanos) {
    // Sample = handler execution time. `finished >= t_received` always
    // (same monotonic clock, finished stamped after received); guard the
    // subtraction anyway so a clock anomaly can't underflow.
    let sample = finished.0.saturating_sub(t_received.0);
    CostCells::try_with_mut(|cells| {
        if let Some(cell) = cells.get(kind) {
            cell.fold(sample);
        }
    });
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic on failure is the assertion"
)]
mod cost_tests {
    use super::*;
    use crate::mail::MailboxId;
    use crate::test_util::fresh_substrate;
    use aether_actor::local::{ActorSlots, with_stamped};
    use aether_kinds::{CostTail, CostTailResult};

    /// iamacoffeepot/aether#1128 step 4: folding a known handler's
    /// execution time moves its cell, and the global `cost.tail` dump
    /// surfaces the moved row.
    #[test]
    fn fold_moves_seeded_handler_cell() {
        let (_registry, mailer) = fresh_substrate();
        let self_mbx = MailboxId(0x1128);
        let handled = KindId(10);
        // Construction seeds both indexes from the handler set — the
        // global table and the actor's per-actor cache, over one shared
        // `Arc<CostCell>`. Reproduce that: seed the table, stamp the
        // returned Arcs into the cache under `with_stamped` (as `init` /
        // the boot wrap do), then fold.
        let slots = ActorSlots::new();
        with_stamped(&slots, || {
            let seeded = mailer.cost_table().seed(self_mbx, &[handled]);
            CostCells::try_with_mut(|cells| cells.seed(seeded));
            fold_handler_cost(handled, Nanos(1_000), Nanos(6_000));
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

        let slots = ActorSlots::new();
        // Seed only `handled` into the cache (as construction does), then
        // fold a kind the actor never declared (a framework / fallback
        // kind) — it must not create a row.
        let stranger = KindId(<LogTail as Kind>::ID.0);
        with_stamped(&slots, || {
            let seeded = mailer.cost_table().seed(self_mbx, &[handled]);
            CostCells::try_with_mut(|cells| cells.seed(seeded));
            fold_handler_cost(stranger, Nanos(0), Nanos(9_999));
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

/// iamacoffeepot/aether#1272: regression coverage for the `NativeCtx`-
/// free framework dispatch arm variants the desktop window driver
/// reaches for from its bespoke inbox drain.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic on failure is the assertion"
)]
mod free_dispatch_tests {
    use super::*;
    use crate::handle_store::HandleStore;
    use crate::mail::mailer::Mailer;
    use crate::mail::outbound::{EgressEvent, HubOutbound};
    use crate::mail::registry::Registry;
    use crate::mail::{MailRef, SourceAddr};
    use aether_actor::local::{ActorSlots, with_stamped};
    use aether_data::{MailId, SessionToken};
    use aether_kinds::SetWindowTitle;
    use aether_kinds::descriptors;
    use aether_kinds::trace::Nanos;
    use std::sync::Arc;
    use std::sync::mpsc;

    fn fresh_substrate_with_outbound() -> (Arc<Mailer>, mpsc::Receiver<EgressEvent>) {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, rx) = HubOutbound::attached_loopback();
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(registry, store).with_outbound(outbound));
        (mailer, rx)
    }

    fn build_envelope<K: Kind>(payload: &K, reply_to: Source) -> Envelope {
        let bytes = payload.encode_into_bytes();
        Envelope::disarmed(
            KindId(<K as Kind>::ID.0),
            <K as Kind>::NAME.to_owned(),
            None,
            reply_to,
            MailRef::from(bytes),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            MailboxId(0),
        )
    }

    /// The free-fn log-tail arm fires a `LogTailResult` reply to the
    /// envelope's `sender` when the kind matches — the property the
    /// desktop window driver (iamacoffeepot/aether#1272) relies on so
    /// `actor_logs aether.window` resolves instead of hanging on a
    /// never-arriving reply.
    #[test]
    fn dispatch_log_tail_free_replies_on_match() {
        let (mailer, rx) = fresh_substrate_with_outbound();
        let session = SessionToken::NIL;
        let reply_to = Source::with_correlation(SourceAddr::Session(session), 0x1272_4242);
        let env = build_envelope(
            &LogTail {
                max: 10,
                min_level: None,
                since: None,
            },
            reply_to,
        );

        // The arm reads `ActorLogRing::try_with`, so it must run inside a
        // `with_stamped` block. Stamping with a fresh slot map makes the
        // ring resolve to its default (empty) — the reply lands as
        // `LogTailResult::Ok { entries: [] }`.
        let slots = ActorSlots::new();
        let matched = with_stamped(&slots, || {
            dispatch_log_tail_if_matching_free(&mailer, reply_to, &env)
        });
        assert!(matched, "log.tail kind matches");

        let event = rx.try_recv().expect("reply egress recorded");
        match event {
            EgressEvent::ToSession {
                session: got_session,
                kind_name,
                correlation_id,
                ..
            } => {
                assert_eq!(got_session, session);
                assert_eq!(kind_name, <LogTailResult as Kind>::NAME);
                assert_eq!(correlation_id, 0x1272_4242);
            }
            other => panic!("expected ToSession egress, got {other:?}"),
        }
    }

    /// Non-matching kinds fall through without replying — the driver's
    /// existing `kind_set_window_mode` / `kind_set_window_title` arms
    /// still get their chance after the framework arms return `false`.
    #[test]
    fn dispatch_log_tail_free_skips_non_match() {
        let (mailer, rx) = fresh_substrate_with_outbound();
        let session = SessionToken::NIL;
        let reply_to = Source::with_correlation(SourceAddr::Session(session), 0);
        // Build an envelope of a different kind (`SetWindowTitle`); the
        // arm must early-return `false` and emit nothing.
        let env = build_envelope(
            &SetWindowTitle {
                title: "test".to_owned(),
            },
            reply_to,
        );

        let slots = ActorSlots::new();
        let matched = with_stamped(&slots, || {
            dispatch_log_tail_if_matching_free(&mailer, reply_to, &env)
        });
        assert!(!matched, "non-log.tail kind doesn't match");
        assert!(rx.try_recv().is_err(), "skip path emits no reply egress");
    }

    /// The trace-tail and cost-tail free fns are siblings of the log-tail
    /// arm; smoke-test both fire their reply on a matched envelope so a
    /// future contract slip on either kind doesn't silently regress
    /// `actor_logs`-style queries against the desktop driver.
    #[test]
    fn dispatch_trace_and_cost_tail_free_reply_on_match() {
        let (mailer, rx) = fresh_substrate_with_outbound();
        let self_mbx = MailboxId(0x1272_AAAA);
        let session = SessionToken::NIL;
        let reply_to = Source::with_correlation(SourceAddr::Session(session), 0xCAFE);

        let trace_env = build_envelope(
            &TraceTail {
                max: 16,
                since: None,
                root: None,
            },
            reply_to,
        );
        let cost_env = build_envelope(&CostTail { kind: None }, reply_to);

        let slots = ActorSlots::new();
        let (trace_match, cost_match) = with_stamped(&slots, || {
            let trace = dispatch_trace_tail_if_matching_free(&mailer, reply_to, &trace_env);
            let cost = dispatch_cost_tail_if_matching_free(&mailer, reply_to, self_mbx, &cost_env);
            (trace, cost)
        });
        assert!(trace_match);
        assert!(cost_match);

        // Two replies must have been routed; both `ToSession`.
        let names: Vec<String> = (0..2)
            .map(|_| match rx.try_recv().expect("reply egress recorded") {
                EgressEvent::ToSession { kind_name, .. } => kind_name,
                other => panic!("expected ToSession egress, got {other:?}"),
            })
            .collect();
        assert!(names.iter().any(|n| n == <TraceTailResult as Kind>::NAME));
        assert!(names.iter().any(|n| n == <CostTailResult as Kind>::NAME));
    }
}
