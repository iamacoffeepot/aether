//! Clock seam: the store stamps `recorded_at_millis`; tests inject a fixed clock.

use std::time::{SystemTime, UNIX_EPOCH};

/// Source of wall-clock milliseconds for entry and artifact stamps.
pub trait Clock {
    /// Current unix time in milliseconds.
    #[must_use]
    fn now_millis(&self) -> u64;
}

/// Host wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|d| u64::try_from(d.as_millis()).ok()).unwrap_or(0)
    }
}
