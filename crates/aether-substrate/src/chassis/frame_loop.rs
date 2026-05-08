//! Shared frame-loop policy helpers (issue 427).
//!
//! Two helpers, both invariant across chassis: the per-frame
//! frame-bound drain barrier (ADR-0074 §Decision 5) and the cadenced
//! `FrameStats` broadcast every 120 frames.
//!
//! - `drain_frame_bound_or_abort` waits on each frame-bound cap's
//!   pending counter under `DRAIN_BUDGET`; a counter that doesn't
//!   reach zero is treated as wedged and routes through
//!   `lifecycle::fatal_abort`.
//! - `emit_frame_stats` does the 120-frame gate inside the helper —
//!   chassis call sites become unconditional.
//!
//! Pre-Phase-4 there was a third helper, `drain_or_abort`, that
//! polled a per-component pending-counter aggregate to detect dead /
//! wedged wasm dispatchers. Issue 634 Phase 4 PR 1 retired the
//! per-component routing path; PR 2 retired the polling barrier in
//! favour of direct trap-abort at the trampoline (the trampoline
//! holds a `FatalAborter` and aborts on `Component::deliver` Err).
//! Wedge detection (CPU-loop wasm guests) waits on a future
//! epoch-deadline ADR — symmetric with native actors, which have
//! no wedge guard either.
//!
//! `WORKERS` deliberately stays chassis-side. Post-ADR-0038 it's
//! declarative (the wire-stable `EngineInfo.workers` field, retained
//! for compatibility — the scheduler doesn't read it). It's not
//! actual loop policy and shouldn't be promoted into a shared
//! module just because every chassis happens to set the same value.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aether_data::encode;
use aether_kinds::FrameStats;

use crate::mail::mailer::Mailer;
use crate::mail::outbound::HubOutbound;
use crate::mail::{Mail, MailboxId};
use crate::runtime::lifecycle;

/// Frame-stats emission cadence. Hardcoded for v1; an env knob is
/// deferred until a forcing function arrives. 120 frames at 60 Hz is
/// 2 s — frequent enough for a Claude session to see liveness via
/// `receive_mail`, sparse enough to stay out of the engine_logs
/// signal-to-noise budget.
pub const LOG_EVERY_FRAMES: u64 = 120;

/// ADR-0063 fail-fast budget for the per-frame drain barrier. A
/// dispatcher that doesn't quiesce within this window is treated as
/// wedged: the substrate logs, broadcasts `SubstrateDying`, and
/// exits via `lifecycle::fatal_abort`. 5 s is patient enough that
/// ordinary frames don't trip it even on slow first-load compiles,
/// short enough that an operator staring at a frozen window gets a
/// clean exit instead of a multi-minute wait.
pub const DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// Wait for every frame-bound capability's inbox to drain under
/// `DRAIN_BUDGET` (ADR-0074 §Decision 5). Works on the per-mailbox
/// pending counters
/// [`crate::ChassisCtx::claim_frame_bound_mailbox`] collected for the
/// chassis (snapshotted by drivers via
/// [`crate::chassis::builder::DriverCtx::frame_bound_pending`]).
///
/// Empty `pending` is a fast no-op — chassis without frame-bound
/// capabilities (today: headless, hub) call this every frame at
/// zero cost.
pub fn drain_frame_bound_or_abort(pending: &[(MailboxId, Arc<AtomicU64>)], outbound: &HubOutbound) {
    if pending.is_empty() {
        return;
    }
    let deadline = Instant::now() + DRAIN_BUDGET;
    loop {
        let mut still_pending: Option<(MailboxId, u64)> = None;
        for (mbox, counter) in pending {
            let v = counter.load(Ordering::Acquire);
            if v > 0 {
                still_pending = Some((*mbox, v));
                break;
            }
        }
        match still_pending {
            None => return,
            Some((mbox, count)) => {
                if Instant::now() >= deadline {
                    let reason = format!(
                        "frame-bound dispatcher wedged: mailbox={mbox} pending={count} waited={DRAIN_BUDGET:?}"
                    );
                    lifecycle::fatal_abort(outbound, reason);
                }
                std::thread::sleep(Duration::from_micros(50));
            }
        }
    }
}

/// Emit a `FrameStats` broadcast every `LOG_EVERY_FRAMES` frames.
/// The cadence gate lives inside the helper so chassis call sites
/// are unconditional — pre-refactor each chassis open-coded the
/// `frame.is_multiple_of(LOG_EVERY_FRAMES)` check.
///
/// Pushes a single 16-byte cast-encoded `FrameStats` to the
/// broadcast mailbox via `queue.push`; observation routing is
/// handled by the registered sink that owns the broadcast name.
/// Fire-and-forget — the broadcast fans out to every attached
/// Claude session, no reply expected. The `tracing::info!` log
/// line is left to the caller because chassis carry chassis-
/// specific context (FPS, elapsed) the helper shouldn't decide
/// the schema for.
///
/// Stage 3 of issue 552 retired the `broadcast_mbox` parameter: the
/// recipient is derived from `aether_kinds::HUB_BROADCAST_MAILBOX_NAME`
/// inline (`mailbox_id_from_name` is `const fn`), matching the
/// broadcast cap's claim under the same constant (issue 576 promoted
/// broadcast into a real chassis cap; the name stayed in
/// `aether-kinds` so substrate-internal pushes don't depend on
/// `aether-capabilities`). `sender` likewise retired — the broadcast
/// path is target-by-mailbox + fan-out, no reply, so a sender
/// identity wasn't read by any consumer.
pub fn emit_frame_stats(
    queue: &Mailer,
    kind_frame_stats: aether_data::KindId,
    frame: u64,
    triangles: u64,
) {
    if !frame.is_multiple_of(LOG_EVERY_FRAMES) {
        return;
    }
    queue.push(Mail::new(
        aether_data::mailbox_id_from_name(aether_kinds::HUB_BROADCAST_MAILBOX_NAME),
        kind_frame_stats,
        encode(&FrameStats { frame, triangles }),
        1,
    ));
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use aether_data::Kind;
    use aether_kinds::FrameStats;

    use super::*;
    use crate::mail::registry::Registry;

    /// `emit_frame_stats` is a no-op on non-multiples of
    /// `LOG_EVERY_FRAMES`. Verified by sending into a sink that
    /// records every payload — a non-multiple frame must produce
    /// zero deliveries.
    #[test]
    fn emit_frame_stats_skips_non_multiples() {
        let registry = Arc::new(Registry::new());
        let captured: Arc<RwLock<Vec<Vec<u8>>>> = Arc::new(RwLock::new(Vec::new()));
        let captured_for_sink = Arc::clone(&captured);
        registry.register_closure(
            aether_kinds::HUB_BROADCAST_MAILBOX_NAME,
            Arc::new(
                move |_kind_id, _kind_name, _origin, _sender, bytes, _count| {
                    captured_for_sink.write().unwrap().push(bytes.to_vec());
                },
            ),
        );
        let mailer = Arc::new(Mailer::new());
        mailer.wire(Arc::clone(&registry));

        let kind_id = FrameStats::ID;
        emit_frame_stats(&mailer, kind_id, 1, 0);
        emit_frame_stats(&mailer, kind_id, 119, 0);
        assert!(captured.read().unwrap().is_empty());
    }

    /// `emit_frame_stats` emits a FrameStats payload on the
    /// `LOG_EVERY_FRAMES` boundary. The captured bytes round-trip
    /// through the cast decoder back to the input values.
    #[test]
    fn emit_frame_stats_emits_on_multiple() {
        let registry = Arc::new(Registry::new());
        let captured: Arc<RwLock<Vec<Vec<u8>>>> = Arc::new(RwLock::new(Vec::new()));
        let captured_for_sink = Arc::clone(&captured);
        registry.register_closure(
            aether_kinds::HUB_BROADCAST_MAILBOX_NAME,
            Arc::new(
                move |_kind_id, _kind_name, _origin, _sender, bytes, _count| {
                    captured_for_sink.write().unwrap().push(bytes.to_vec());
                },
            ),
        );
        let mailer = Arc::new(Mailer::new());
        mailer.wire(Arc::clone(&registry));

        emit_frame_stats(&mailer, FrameStats::ID, LOG_EVERY_FRAMES, 42);
        let frames = captured.read().unwrap();
        assert_eq!(frames.len(), 1, "one delivery on the boundary");
        let stats: FrameStats = aether_data::decode(&frames[0]).expect("decode FrameStats");
        assert_eq!(stats.frame, LOG_EVERY_FRAMES);
        assert_eq!(stats.triangles, 42);
    }
}
