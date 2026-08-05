//! The settle pump's poll-granularity knob (issue 4453), sitting beside
//! [`crate::settlement_config`] because the two govern the same wait from
//! opposite ends: that one bounds how long the pump is willing to wait at
//! all, this one how finely it looks while waiting.

use std::time::Duration;

/// Default poll ceiling, in microseconds. Ten milliseconds — coarse
/// enough that a wait on a slow capability (`FsCapability` polls its
/// inbox at 100 ms) sleeps rather than pinning a core. The literal
/// `default = 10_000` on [`PollConfig`] must equal this.
const DEFAULT_POLL_CAP_MICROS: u64 = 10_000;

/// Poll-granularity knob for the settle pump (issue 4453). The pump
/// resets its backoff to a 50 µs floor on any drained event and doubles
/// toward this ceiling only on genuine quiet. A frame's exact lifecycle
/// chain remains at that floor while it is outstanding (issue 4454), so a
/// long silent handler is not measured through this coarse ceiling. The
/// ceiling applies once no exact chain is known outstanding, including
/// slow capability and reply-only waits.
///
/// The `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `PollConfigLayer`, the clap-shaped `PollOverlay`, the `FromArgvThenEnv`
/// impl, and the inherent `from_env` / `from_argv_then_env` shims
/// (ADR-0090 unit g). Resolved once at harness construction and lowered
/// via [`Self::to_cap`] to the `Duration` the pump reads.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_HARNESS_POLL", cli_prefix = "harness-poll")]
pub struct PollConfig {
    /// Microseconds the pump's quiet backoff may reach; 0 restores the default.
    ///
    /// Lowering it shrinks observation lag for untracked and
    /// post-settlement quiet and pays CPU for that resolution.
    #[config(env = "AETHER_HARNESS_POLL_CAP_MICROS", default = 10_000)]
    pub cap_micros: u64,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self { cap_micros: DEFAULT_POLL_CAP_MICROS }
    }
}

impl PollConfig {
    /// Lower the resolved knob to the ceiling the pump clamps its backoff
    /// to.
    ///
    /// `0` restores the default rather than meaning "no ceiling": the
    /// pump sleeps `backoff` and then clamps to this value, so a zero
    /// collapses the backoff into a spin that pins a core and starves the
    /// dispatcher threads the wait is waiting on — the opposite of what
    /// reaching for this knob is ever for. Refusing it here is why the
    /// pump itself needs no guard.
    #[must_use]
    pub fn to_cap(&self) -> Duration {
        if self.cap_micros == 0 {
            Duration::from_micros(DEFAULT_POLL_CAP_MICROS)
        } else {
            Duration::from_micros(self.cap_micros)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: `0` must not lower to a zero-duration ceiling. The pump
    /// clamps its sleep to this, so a zero turns every quiet poll into a
    /// spin — a hang that presents as a mysteriously slow scenario rather
    /// than a failure, and one no other gate would catch. The
    /// microseconds pin catches the unit slip that would leave the knob
    /// wrong by 1000x while still appearing to work.
    #[test]
    fn a_zero_cap_lowers_to_the_default_rather_than_a_spin() {
        assert_eq!(PollConfig { cap_micros: 0 }.to_cap(), Duration::from_micros(DEFAULT_POLL_CAP_MICROS));
        assert_eq!(PollConfig { cap_micros: 250 }.to_cap(), Duration::from_micros(250), "the knob is microseconds");
        assert_eq!(PollConfig::default().to_cap(), Duration::from_micros(DEFAULT_POLL_CAP_MICROS));
    }
}
