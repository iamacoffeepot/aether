//! Poll-quantization accounting for the settle pump (issue 4453).
//!
//! `pump_until_event` observes a settled chain only when one of its
//! sleeps ends, so a wait's measured duration is the truth rounded up
//! to a poll boundary. On a quiet wait the backoff doubles from 50 µs
//! to a cap, which means a long silent handler — the orbit re-split is
//! ~15 ms of wasm that emits nothing until it returns — is observed up
//! to a whole capped sleep late. Attributing the orbit frame requires
//! telling that observation lag apart from the work, so the pump
//! records where it slept and the cap is made settable.
//!
//! Process-global rather than a harness field because the pump is the
//! only writer and a measuring run is one harness on one thread; the
//! alternative threads a counter through every op signature for a
//! number only an instrument reads.

use std::env;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static SLEPT_NANOS: AtomicU64 = AtomicU64::new(0);
static SLEEPS: AtomicU64 = AtomicU64::new(0);
static CAPPED_SLEEPS: AtomicU64 = AtomicU64::new(0);
static LAST_SLEEP_NANOS: AtomicU64 = AtomicU64::new(0);

/// Where one measured span's pump went while it waited.
///
/// `slept` is time the pumping thread was not polling — the work
/// itself runs on dispatcher threads meanwhile, so this is not idle
/// time in the engine, only in the observer. `last_sleep` is the
/// sleep the awaited reply was found at the end of, so it bounds how
/// much later than the truth the observation landed.
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
}

/// Record one pump sleep. Called from the pump's quiet branch.
pub(crate) fn record_sleep(slept: Duration, at_cap: bool) {
    let nanos = u64::try_from(slept.as_nanos()).unwrap_or(u64::MAX);
    SLEPT_NANOS.fetch_add(nanos, Ordering::Relaxed);
    SLEEPS.fetch_add(1, Ordering::Relaxed);
    if at_cap {
        CAPPED_SLEEPS.fetch_add(1, Ordering::Relaxed);
    }
    LAST_SLEEP_NANOS.store(nanos, Ordering::Relaxed);
}

/// Read and reset the counters, so an instrument brackets one op by
/// calling this on either side of it.
#[must_use]
pub fn take() -> PumpStats {
    PumpStats {
        slept: Duration::from_nanos(SLEPT_NANOS.swap(0, Ordering::Relaxed)),
        sleeps: SLEEPS.swap(0, Ordering::Relaxed),
        capped_sleeps: CAPPED_SLEEPS.swap(0, Ordering::Relaxed),
        last_sleep: Duration::from_nanos(LAST_SLEEP_NANOS.swap(0, Ordering::Relaxed)),
    }
}

/// The ceiling that keeps a wait on a slow capability
/// (`FsCapability` polls its inbox at 100 ms) sleeping coarsely rather
/// than pinning a core.
const DEFAULT_CAP: Duration = Duration::from_millis(10);

/// Resolve the ceiling from a raw override, falling back to
/// [`DEFAULT_CAP`].
///
/// Zero is refused rather than honoured: a zero ceiling collapses the
/// backoff into a spin, which pins a core and starves the very
/// dispatcher threads the wait is waiting on — the opposite of what
/// someone reaching for this knob wants. Anything unparseable falls
/// back for the same reason.
fn parse_cap(raw: Option<&str>) -> Duration {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|micros| *micros > 0)
        .map_or(DEFAULT_CAP, Duration::from_micros)
}

/// The pump's backoff ceiling, resolved once per process. An instrument
/// lowers it to shrink observation lag, paying CPU for the resolution.
pub fn backoff_cap() -> Duration {
    static CAP: OnceLock<Duration> = OnceLock::new();
    *CAP.get_or_init(|| {
        // Dev/perf tooling: a process-level resolution knob for the
        // measuring harness itself, not capability config — there is no
        // cap whose ADR-0090 layer this would belong to.
        #[allow(clippy::disallowed_methods)]
        let raw = env::var("AETHER_HARNESS_POLL_CAP_MICROS").ok();
        parse_cap(raw.as_deref())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: a `0` override must not become a zero-duration ceiling.
    /// The pump sleeps `backoff` and then clamps to this value, so a zero
    /// here turns every quiet poll into a spin that pins a core and
    /// starves the dispatcher threads the wait depends on — a hang that
    /// presents as a mysteriously slow test rather than as a failure.
    /// The micros-vs-millis pin catches the unit slip that would make the
    /// knob wrong by 1000x while still looking like it works.
    #[test]
    fn a_zero_or_unparseable_override_falls_back_rather_than_spinning() {
        assert_eq!(parse_cap(Some("0")), DEFAULT_CAP, "zero must not become a spin");
        assert_eq!(parse_cap(Some("")), DEFAULT_CAP);
        assert_eq!(parse_cap(Some("fast")), DEFAULT_CAP);
        assert_eq!(parse_cap(None), DEFAULT_CAP);

        assert_eq!(parse_cap(Some("250")), Duration::from_micros(250), "the override is microseconds");
        assert_eq!(parse_cap(Some(" 250 ")), Duration::from_micros(250));
    }
}
