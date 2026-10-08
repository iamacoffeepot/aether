//! The limit on the game time one desktop frame adds. The desktop driver is
//! the one driver that measures time: a stalled frame, or any gap between two
//! frames, reaches it as one long interval. It holds each frame's share to a
//! limit before stating it to the lifecycle capability, so game time slows
//! across a stall and no step is skipped (ADR-0082, 2026-10-06 amendment).

use std::num::NonZeroU32;
use std::time::Duration;

/// Minimum spacing between warnings of uncounted frame time. A stall that
/// lasts logs once, never once per frame.
pub(super) const STALL_WARN_COOLDOWN: Duration = Duration::from_secs(5);

/// The most game time one frame adds, in microseconds. Never zero: a limit
/// of zero would stop game time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameDeltaLimit(NonZeroU32);

/// One frame's measured interval split at the limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameDelta {
    /// The microseconds the frame adds to game time.
    pub counted_micros: u32,
    /// The measured microseconds past the limit, which game time drops.
    pub uncounted_micros: u64,
}

impl FrameDeltaLimit {
    /// A limit of `micros` microseconds, or `None` for zero.
    #[must_use]
    pub const fn from_micros(micros: u32) -> Option<Self> {
        match NonZeroU32::new(micros) {
            Some(micros) => Some(Self(micros)),
            None => None,
        }
    }

    /// Split the interval a frame measured into the part game time counts
    /// and the part it drops. The interval is limited before it narrows to
    /// `u32`, so one longer than `u32::MAX` microseconds counts the limit.
    #[must_use]
    pub fn limit(self, measured: Duration) -> FrameDelta {
        let limit_micros = self.0.get();
        let measured_micros = u64::try_from(measured.as_micros()).unwrap_or(u64::MAX);
        let counted_micros = u32::try_from(measured_micros).map_or(limit_micros, |micros| micros.min(limit_micros));

        FrameDelta { counted_micros, uncounted_micros: measured_micros - u64::from(counted_micros) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The limit holds a measured interval to itself and reports the rest.
    /// An interval narrowed to `u32` before it is limited would wrap two
    /// hours into a short frame, and a `<` in place of `<=` would drop a
    /// microsecond from a frame exactly at the limit. Every duration is
    /// stated; no clock is read.
    #[test]
    fn limit_counts_up_to_the_limit_and_reports_the_rest() {
        let limit = FrameDeltaLimit::from_micros(250_000).expect("a nonzero limit");
        let two_hours = Duration::from_hours(2);

        assert_eq!(
            limit.limit(Duration::from_micros(16_667)),
            FrameDelta { counted_micros: 16_667, uncounted_micros: 0 },
            "a frame under the limit is counted whole"
        );
        assert_eq!(
            limit.limit(Duration::from_millis(250)),
            FrameDelta { counted_micros: 250_000, uncounted_micros: 0 },
            "a frame at the limit is counted whole"
        );
        assert_eq!(
            limit.limit(Duration::from_micros(250_001)),
            FrameDelta { counted_micros: 250_000, uncounted_micros: 1 },
            "one microsecond over the limit is the one left uncounted"
        );
        assert_eq!(
            limit.limit(two_hours),
            FrameDelta { counted_micros: 250_000, uncounted_micros: 7_200_000_000 - 250_000 },
            "two hours is more microseconds than a u32 holds"
        );
    }
}
