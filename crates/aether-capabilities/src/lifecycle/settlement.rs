//! The settlement-gated advance state machine (ADR-0082 §6). Carries
//! the in-flight [`PendingAdvance`], the edge-resolution + force-complete
//! decision logic, and the rolling settlement-latency EWMA with its
//! slow-warn gate (iamacoffeepot/aether#1048 / #1052). Runtime-only — the
//! whole settlement path sits behind the `feature = "runtime"` gate (the
//! `mod settlement;` declaration carries it), alongside the rest of the
//! `LifecycleCapability` runtime half (ADR-0122).

use std::time::{Duration, Instant};

use aether_actor::Manual;
use aether_actor::actor::ctx::OutboundReply;
use aether_data::KindId;
use aether_kinds::LifecycleAdvanceComplete;
use aether_substrate::actor::native::NativeCtx;
use aether_substrate::mail::{MailId, Source};

use super::LifecycleStateData;
use super::runtime::LifecycleCapabilityState;

/// Default deadline for a pending advance's `Settled` to arrive
/// before [`LifecycleCapability::on_advance`](super) force-completes it
/// (iamacoffeepot/aether#1048). Override via
/// `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS`. Generous relative to the
/// ~16 ms frame tick: normal settlement is sub-tick, so this only
/// fires when the settlement pipeline has actually stalled —
/// degrading a permanent wedge into a visible stutter rather than
/// tripping on ordinary jitter.
pub const ADVANCE_TIMEOUT_MS_DEFAULT: u64 = 1_000;

/// Early-warning threshold for slow settlement
/// (iamacoffeepot/aether#1052, the prevention follow-up to #1048). A
/// `Sent`→`Settled` latency past `advance_timeout / SLOW_SETTLE_DIVISOR`
/// (≈100ms at the 1s default, ~6 frames at 60Hz) is well above the
/// sub-tick norm but a full 10× short of the force-complete deadline,
/// so the warn surfaces a degrading settlement pipeline *before* it
/// wedges, with headroom to act.
pub const SLOW_SETTLE_DIVISOR: u32 = 10;

/// EWMA smoothing factor for the rolling settlement-latency stat, as
/// a permille (200 ‰ = 0.2). A single spike moves the average ~20%.
pub const SETTLE_EWMA_ALPHA_PERMILLE: u32 = 200;

/// Minimum spacing between slow-settlement warns. A saturating
/// pipeline settles slowly on *every* advance, so an unguarded warn
/// would itself spam the rings; one line per episode is enough.
pub const SLOW_SETTLE_WARN_COOLDOWN: Duration = Duration::from_secs(5);

/// Internal state-advance decision produced by `on_advance` before
/// the cap mutates its own fields. Declared at module scope to keep
/// the handler body statement-only (`clippy::items_after_statements`).
pub enum Step {
    StateAdvance { broadcast: KindId, next: KindId },
    Terminal { broadcast: KindId },
    Unknown,
}

/// Per-advance state tracked across `on_advance` → `on_settled`.
pub struct PendingAdvance {
    /// Causal-chain root of the in-flight broadcast (ADR-0080 §6).
    pub(crate) root: MailId,
    /// Kind id of the state just broadcast — echoed in `completed`.
    pub(crate) completed_kind: KindId,
    /// Kind id of the state to broadcast next — echoed in `next`.
    /// `KindId(0)` when the settling broadcast was a terminal.
    pub(crate) next_kind: KindId,
    /// True if the settling broadcast is a terminal state.
    pub(crate) is_terminal: bool,
    /// Original chassis sender of the [`LifecycleAdvance`](aether_kinds::LifecycleAdvance) mail.
    pub(crate) reply_to: Source,
    /// When this advance was issued. Drives the `advance_timeout`
    /// force-complete fallback (iamacoffeepot/aether#1048).
    pub(crate) started: Instant,
}

impl LifecycleCapabilityState {
    /// Fold one observed `Sent`→`Settled` latency into the rolling
    /// EWMA and emit a rate-limited warn when a settle blows past the
    /// slow threshold (`advance_timeout / SLOW_SETTLE_DIVISOR`). The
    /// early-warning for a degrading settlement pipeline
    /// (iamacoffeepot/aether#1052, the prevention follow-up to
    /// #1048): it fires with ~10× headroom before the force-complete
    /// deadline, naming the offending `root` so a
    /// `describe_tree <root>` surfaces the in-flight nodes. O(1) per
    /// settle.
    pub(crate) fn record_settlement_latency(&mut self, latency: Duration, root: MailId) {
        // EWMA in nanos, α = SETTLE_EWMA_ALPHA_PERMILLE/1000. Up and
        // down moves are handled separately so the whole thing stays
        // in u128 (no signed casts): next = prev ± α·|sample − prev|.
        let alpha = u128::from(SETTLE_EWMA_ALPHA_PERMILLE);
        let next_nanos = self
            .settlement_latency_ewma
            .map_or(latency.as_nanos(), |prev| {
                let prev = prev.as_nanos();
                let sample = latency.as_nanos();
                if sample >= prev {
                    prev + (sample - prev) * alpha / 1000
                } else {
                    prev - (prev - sample) * alpha / 1000
                }
            });
        let ewma = Duration::from_nanos(u64::try_from(next_nanos).unwrap_or(u64::MAX));
        self.settlement_latency_ewma = Some(ewma);

        let threshold = self.advance_timeout / SLOW_SETTLE_DIVISOR;
        if latency < threshold {
            return;
        }
        if self
            .last_slow_warn
            .is_some_and(|t| t.elapsed() < SLOW_SETTLE_WARN_COOLDOWN)
        {
            return;
        }
        self.last_slow_warn = Some(Instant::now());
        tracing::warn!(
            target: "aether_capabilities::lifecycle",
            root = ?root,
            latency_millis = latency.as_millis(),
            ewma_millis = ewma.as_millis(),
            threshold_millis = threshold.as_millis(),
            "settlement latency exceeded the slow threshold; the trace/settlement \
             pipeline is degrading — `describe_tree <root>` for the in-flight nodes; \
             a sustained climb wedges the lifecycle (iamacoffeepot/aether#1048)"
        );
    }

    /// True when a pending advance has exceeded
    /// [`Self::advance_timeout`](super) without settling
    /// (iamacoffeepot/aether#1048). `false` when nothing is pending.
    pub(crate) fn pending_timed_out(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|p| p.started.elapsed() >= self.advance_timeout)
    }

    /// Force-complete a pending advance whose [`Settled`](aether_kinds::trace::Settled)
    /// never arrived (iamacoffeepot/aether#1048). Mirrors `on_settled`'s
    /// state mutation + reply but logs at `error`: reaching here means
    /// the settlement pipeline stalled past `advance_timeout`. No-op when
    /// nothing is pending.
    pub(crate) fn force_complete_pending(&mut self, ctx: &mut NativeCtx<'_, Manual>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        tracing::error!(
            target: "aether_capabilities::lifecycle",
            root = ?pending.root,
            elapsed_millis = pending.started.elapsed().as_millis(),
            timeout_millis = self.advance_timeout.as_millis(),
            "LifecycleAdvance settlement timed out; force-advancing to avoid a permanent wedge \
             (settlement pipeline may be saturated — see iamacoffeepot/aether#1048)"
        );
        if pending.is_terminal {
            self.terminal_reached = true;
        } else {
            self.current_state = pending.next_kind;
        }
        ctx.reply_to(
            pending.reply_to,
            &LifecycleAdvanceComplete {
                completed: pending.completed_kind.0,
                next: pending.next_kind.0,
            },
        );
    }
}

/// Decide which edge to follow out of `state` given the current
/// `quit_pending` flag (ADR-0082 §3). If `quit_pending` is set AND
/// the state declares a `quit` edge, consume the flag and return the
/// quit target; otherwise return the unconditional `next` target.
pub fn resolve_edge(state: &LifecycleStateData, quit_pending: &mut bool) -> KindId {
    if *quit_pending && let Some(quit_target) = state.quit {
        *quit_pending = false;
        return quit_target;
    }
    state.next
}

#[cfg(test)]
mod tests {
    //! Unit-level tests for the cap's decision logic. End-to-end
    //! broadcast / advance flow is covered by the `test_bench`
    //! frame-loop scenarios; the decision functions below carry the
    //! ADR-0082 §3 quit-flag semantics and the #1048/#1052
    //! settlement-latency gate, pinned at the unit layer.
    use super::super::test_cap;
    use super::*;
    use aether_data::Kind;
    use aether_kinds::{Present, Render};

    fn state_with_quit(kind_id: u64, next: u64, quit: Option<u64>) -> LifecycleStateData {
        LifecycleStateData {
            kind: KindId(kind_id),
            next: KindId(next),
            quit: quit.map(KindId),
        }
    }

    #[test]
    fn resolve_edge_takes_next_when_no_quit_pending() {
        let state = state_with_quit(1, 2, Some(99));
        let mut quit = false;
        assert_eq!(resolve_edge(&state, &mut quit), KindId(2));
        assert!(!quit);
    }

    #[test]
    fn resolve_edge_takes_quit_when_pending_and_declared() {
        let state = state_with_quit(1, 2, Some(99));
        let mut quit = true;
        assert_eq!(resolve_edge(&state, &mut quit), KindId(99));
        assert!(!quit, "quit flag must be consumed");
    }

    #[test]
    fn resolve_edge_persists_quit_when_no_quit_edge_declared() {
        // ADR-0082 §3: the flag persists across states with no
        // declared quit edge; only states declaring `.quit::<K>()`
        // consume it.
        let state = state_with_quit(1, 2, None);
        let mut quit = true;
        assert_eq!(resolve_edge(&state, &mut quit), KindId(2));
        assert!(quit, "quit flag must persist when state has no quit edge");
    }

    #[test]
    fn pending_timeout_predicate() {
        let mut cap = test_cap(Duration::ZERO);
        assert!(!cap.pending_timed_out());
        cap.pending = Some(PendingAdvance {
            root: MailId::NONE,
            completed_kind: <Render as Kind>::ID,
            next_kind: <Present as Kind>::ID,
            is_terminal: false,
            reply_to: Source::NONE,
            started: Instant::now(),
        });
        // Zero timeout: any elapsed >= 0 trips immediately.
        assert!(cap.pending_timed_out());
        // A long timeout never trips on a freshly-issued advance.
        cap.advance_timeout = Duration::from_hours(1);
        assert!(!cap.pending_timed_out());
    }

    #[test]
    fn settlement_latency_ewma_and_slow_warn_gate() {
        // advance_timeout 1s → slow threshold = 1s / 10 = 100ms.
        let mut cap = test_cap(Duration::from_secs(1));

        // First sample seeds the EWMA exactly.
        cap.record_settlement_latency(Duration::from_millis(10), MailId::NONE);
        assert_eq!(cap.settlement_latency_ewma, Some(Duration::from_millis(10)));
        assert!(cap.last_slow_warn.is_none());

        // Second sample moves the EWMA toward it by α=0.2:
        // 10ms + 0.2·(20ms − 10ms) = 12ms.
        cap.record_settlement_latency(Duration::from_millis(20), MailId::NONE);
        assert_eq!(cap.settlement_latency_ewma, Some(Duration::from_millis(12)));
        assert!(cap.last_slow_warn.is_none());

        // A settle past the 100ms threshold arms the warn + cooldown.
        cap.record_settlement_latency(Duration::from_millis(250), MailId::NONE);
        assert!(cap.last_slow_warn.is_some());
        let armed_at = cap.last_slow_warn.expect("warn armed");

        // A second slow settle inside the cooldown does not re-arm.
        cap.record_settlement_latency(Duration::from_millis(300), MailId::NONE);
        assert_eq!(
            cap.last_slow_warn.expect("still armed"),
            armed_at,
            "cooldown should suppress the second warn"
        );
    }
}
