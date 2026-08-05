//! Poll accounting for the settle pump (issue 4453).
//!
//! `pump_until_event` observes a reply only when one of its sleeps ends,
//! so a wait's measured duration is the truth rounded up to a poll
//! boundary. Exact lifecycle chains stay at the fine floor while
//! outstanding (issue 4454); untracked and post-settlement waits may
//! still reach the configured cap. Attributing a frame requires telling
//! that observation lag apart from the work, which means the pump has to
//! say where it slept — this is what it says.
//!
//! Per-harness state rather than process-global counters: the pump is a
//! `&mut self` method, so the alternative costs a field rather than a
//! signature change, and globals would have two harnesses in one test
//! binary silently sharing (and resetting) each other's numbers.

use std::mem;
use std::time::Duration;

/// Where one measured span's pump went while it waited.
///
/// `slept` is time the pumping thread was not polling — the work itself
/// runs on dispatcher threads meanwhile, so this is idle time in the
/// observer, not in the engine. `last_sleep` is the sleep the awaited
/// reply was found at the end of, so it bounds how much later than the
/// truth the observation landed.
#[derive(Clone, Copy, Debug, Default)]
pub struct PumpStats {
    pub slept: Duration,
    pub sleeps: u64,
    pub capped_sleeps: u64,
    pub last_sleep: Duration,
}

impl PumpStats {
    /// Upper bound on the span's observation lag: the reply arrived
    /// somewhere inside the final sleep, so no more than its whole
    /// duration separates the truth from the measurement.
    #[must_use]
    pub fn overshoot_bound(&self) -> Duration {
        self.last_sleep
    }

    /// Record one pump sleep. Called from the pump's quiet branch.
    pub(crate) fn record_sleep(&mut self, slept: Duration, at_cap: bool) {
        self.slept += slept;
        self.sleeps += 1;
        if at_cap {
            self.capped_sleeps += 1;
        }
        self.last_sleep = slept;
    }

    /// Read and reset, so an instrument brackets one op by calling this
    /// on either side of it.
    pub(crate) fn take(&mut self) -> Self {
        mem::take(self)
    }
}
