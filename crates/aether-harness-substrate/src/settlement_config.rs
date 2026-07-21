//! The settlement-patience knob (issue 2062), rehomed beside its
//! primary consumer — the harness's settlement gates — by the crate
//! extraction (issue #3765). The chassis bundle re-imports it for the
//! teardown-budget resolution, so one knob covers both.

use std::time::Duration;

/// Default cumulative settlement-patience cap, in seconds (issue 2062).
/// Five minutes — a generous deadlock/livelock backstop a healthy chain
/// never reaches even on a saturated box, not the gate a healthy chain
/// meets. The literal `default = 300` on [`SettlementConfig`] must equal
/// this.
const DEFAULT_SETTLEMENT_CAP_SECS: u64 = 300;

/// Settlement-patience backstop knob (issue 2062). The harness's settlement
/// gates block on the settlement signal and treat this cap as a generous
/// deadlock/livelock backstop, not the 30 s wall-clock correctness gate
/// that false-fired under `nextest --workspace` saturation (a healthy-but-
/// slow chain settling at e.g. 45 s was wrongly declared wedged). The
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `SettlementConfigLayer`, the clap-shaped `SettlementOverlay`, the
/// `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` shims (ADR-0090 unit g). Resolved once at gate
/// construction and lowered via [`Self::to_cap`] to the `Duration` the
/// harness reads.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_SETTLEMENT", cli_prefix = "settlement")]
pub struct SettlementConfig {
    /// Seconds to wait for a chain to settle before it is declared wedged; 0 waits forever.
    ///
    /// A cumulative settlement-patience backstop (default
    /// `DEFAULT_SETTLEMENT_CAP_SECS`). `0` is the sentinel for "no cap —
    /// wait forever," for attaching a debugger to a suspected deadlock; in
    /// that mode the per-round warn log stays the live signal.
    #[config(env = "AETHER_SETTLEMENT_CAP_SECS", default = 300)]
    pub cap_secs: u64,
}

impl Default for SettlementConfig {
    fn default() -> Self {
        Self { cap_secs: DEFAULT_SETTLEMENT_CAP_SECS }
    }
}

impl SettlementConfig {
    /// Lower the resolved knob to the cumulative-cap [`Duration`] the
    /// settlement gates read. `0` maps to [`Duration::MAX`] — the
    /// "no cap" sentinel, which the gate's `waited >= cap` test never
    /// trips, so the wait blocks on the signal forever.
    #[must_use]
    pub fn to_cap(&self) -> Duration {
        if self.cap_secs == 0 {
            Duration::MAX
        } else {
            Duration::from_secs(self.cap_secs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settlement_to_cap_maps_seconds_and_zero_sentinel() {
        // Issue 2062 — the only logic this knob owns: seconds → `Duration`,
        // with `0` as the "no cap — wait forever" sentinel. Constructed
        // directly, so the test exercises our `to_cap`, not confique's
        // env/argv resolution (which the derive macro generates and
        // confique's own tests cover).
        assert_eq!(SettlementConfig { cap_secs: 0 }.to_cap(), Duration::MAX, "0 → wait forever");
        assert_eq!(SettlementConfig { cap_secs: 45 }.to_cap(), Duration::from_secs(45),);
        assert_eq!(SettlementConfig::default().to_cap(), Duration::from_secs(DEFAULT_SETTLEMENT_CAP_SECS),);
    }
}
