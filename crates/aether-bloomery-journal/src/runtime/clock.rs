//! Clock seam: the store stamps `recorded_at_millis`; tests inject a fixed clock.

use std::sync::Arc;
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

/// One shared clock: the journal and the bundle driver read the same one, so
/// a test that injects a clock injects it for both (ADR-0245).
impl<C: Clock + ?Sized> Clock for Arc<C> {
    fn now_millis(&self) -> u64 {
        (**self).now_millis()
    }
}
