//! Shared fixed clock for journal tests.

use aether_bloomery_journal::Clock;

/// Clock that always returns the milliseconds it was constructed with.
pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.0
    }
}
