//! The trace clock: [`Nanos`] since a boot anchor, read several times per
//! dispatched mail (`Received`, the first send's construct start, the flush,
//! `Finished`, a burst pickup).
//!
//! `Instant::now()` costs ~29ns on a 16-core Zen 2 desktop (3950X) against
//! ~9ns for a bare `rdtsc`, so at five reads per relay hop the clock alone was
//! ~10% of a mail's CPU (iamacoffeepot/aether#7138). On `x86_64` Linux whose
//! kernel clocksource is `tsc` — the kernel keeps that clocksource only when
//! the TSC is invariant and synchronized across cores — the clock reads the
//! TSC directly and scales ticks to nanoseconds by a ratio calibrated once per
//! process against `Instant`. Everywhere else it reads `Instant`.
//!
//! Trace timestamps are compared only with one another (spans, EWMA folds,
//! trace-tree ordering), never with an `Instant` taken elsewhere, so a
//! calibration error of a few parts per million shows up as a proportional
//! error in a span, not a skew between sources.

use std::time::Instant;

use aether_kinds::trace::Nanos;

/// A boot-anchored monotonic nanosecond clock. `Copy`: the anchor is two
/// plain readings, and every read goes to the process-global source.
#[derive(Clone, Copy, Debug)]
pub struct TraceClock {
    boot: Instant,
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    boot_ticks: Option<u64>,
}

impl TraceClock {
    /// Anchor a clock at the current instant.
    #[must_use]
    pub fn start() -> Self {
        Self {
            boot: Instant::now(),
            #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
            boot_ticks: tsc::scale().map(|_| tsc::ticks()),
        }
    }

    /// The `Instant` this clock was anchored at.
    #[must_use]
    pub fn boot_instant(&self) -> Instant {
        self.boot
    }

    /// Nanoseconds since the anchor. Saturates at zero should a read land on
    /// a core whose counter trails the anchoring core's.
    #[must_use]
    pub fn now_nanos(&self) -> Nanos {
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        if let (Some(boot_ticks), Some(scale)) = (self.boot_ticks, tsc::scale()) {
            return Nanos(scale.nanos(tsc::ticks().saturating_sub(boot_ticks)));
        }
        // u128 → u64: trace timestamps overflow after ~584 years of uptime.
        #[allow(clippy::cast_possible_truncation)]
        Nanos(Instant::now().saturating_duration_since(self.boot).as_nanos() as u64)
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod tsc {
    use std::arch::x86_64::_rdtsc;
    use std::fs;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    /// How long calibration watches both clocks. One millisecond puts the
    /// ratio within a few parts per million: `Instant`'s ~30ns read jitter
    /// over the window.
    const CALIBRATION: Duration = Duration::from_millis(1);

    /// Ticks → nanoseconds as a 32.32 fixed-point multiplier.
    #[derive(Clone, Copy, Debug)]
    pub(super) struct Scale {
        nanos_per_tick_fixed: u64,
    }

    impl Scale {
        pub(super) fn nanos(self, ticks: u64) -> u64 {
            let nanos = (u128::from(ticks) * u128::from(self.nanos_per_tick_fixed)) >> 32;
            u64::try_from(nanos).unwrap_or(u64::MAX)
        }
    }

    pub(super) fn ticks() -> u64 {
        // SAFETY: `rdtsc` is present on every x86_64 CPU and has no memory
        // effects; it only reads the timestamp counter.
        unsafe { _rdtsc() }
    }

    /// The process's calibrated scale, or `None` when the kernel is not
    /// timing off the TSC (a VM without a stable TSC, a machine whose TSC
    /// failed the kernel's synchronization checks).
    pub(super) fn scale() -> Option<Scale> {
        static SCALE: OnceLock<Option<Scale>> = OnceLock::new();
        *SCALE.get_or_init(|| kernel_times_off_tsc().then(calibrate).flatten())
    }

    fn kernel_times_off_tsc() -> bool {
        fs::read_to_string("/sys/devices/system/clocksource/clocksource0/current_clocksource")
            .is_ok_and(|source| source.trim() == "tsc")
    }

    fn calibrate() -> Option<Scale> {
        let (start, start_ticks) = (Instant::now(), ticks());
        let (elapsed, end_ticks) = loop {
            let elapsed = start.elapsed();
            if elapsed >= CALIBRATION {
                break (elapsed, ticks());
            }
        };
        let tick_span = end_ticks.checked_sub(start_ticks).filter(|&span| span > 0)?;
        let fixed = (elapsed.as_nanos() << 32) / u128::from(tick_span);
        Some(Scale { nanos_per_tick_fixed: u64::try_from(fixed).ok()? })
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;

    /// The TSC path's scale must agree with `Instant`: a span the clock
    /// reports tracks the wall span it covered. A wrong fixed-point shift or
    /// an inverted ratio lands orders of magnitude off.
    #[test]
    fn a_span_tracks_the_wall_span_it_covered() {
        let clock = TraceClock::start();
        let wall = Instant::now();
        let before = clock.now_nanos();
        thread::sleep(Duration::from_millis(20));
        let after = clock.now_nanos();
        let wall_nanos = u64::try_from(wall.elapsed().as_nanos()).expect("fits");

        assert!(after.0 >= before.0, "the clock never runs backwards");
        let span = after.0 - before.0;
        assert!(span.abs_diff(wall_nanos) < wall_nanos / 20, "clock span {span}ns vs wall {wall_nanos}ns");
    }
}
