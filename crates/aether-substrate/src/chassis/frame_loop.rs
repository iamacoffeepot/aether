//! Shared frame-loop policy helpers (issue 427).
//!
//! One helper, invariant across chassis: the per-frame frame-bound
//! drain barrier (ADR-0074 §Decision 5). `drain_frame_bound_or_abort`
//! waits on each frame-bound cap's pending counter under
//! `DRAIN_BUDGET`; a counter that doesn't reach zero is treated as
//! wedged and routes through `lifecycle::fatal_abort`.
//!
//! Issue 775 retired the second helper, `emit_frame_stats`, that
//! pushed a `FrameStats` cast every `LOG_EVERY_FRAMES` to the
//! `hub.claude.broadcast` mailbox: with `BroadcastCapability` gone
//! the fan-out has nowhere to land.
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

use crate::mail::MailboxId;
use crate::mail::outbound::HubOutbound;
use crate::runtime::lifecycle;
use std::thread;

/// ADR-0063 fail-fast budget for the per-frame drain barrier. A
/// dispatcher that doesn't quiesce within this window is treated as
/// wedged: the substrate logs and exits via `lifecycle::fatal_abort`.
/// 5 s is patient enough that ordinary frames don't trip it even on
/// slow first-load compiles, short enough that an operator staring
/// at a frozen window gets a clean exit instead of a multi-minute
/// wait.
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
                thread::sleep(Duration::from_micros(50));
            }
        }
    }
}
