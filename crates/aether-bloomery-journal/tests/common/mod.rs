//! Shared fixed clock. Test event kinds live in the files that use them so
//! each integration binary does not carry unused items.

use aether_bloomery_journal::{Clock, Journal, JournalError};

/// Wall-clock stamp every in-memory test uses.
pub const FIXED_MILLIS: u64 = 1_700_000_000_000;

/// Clock that always returns [`FIXED_MILLIS`].
pub struct FixedClock;

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        FIXED_MILLIS
    }
}

/// In-memory journal on the fixed clock.
pub fn journal() -> Result<Journal, JournalError> {
    Journal::open_in_memory_with_clock(Box::new(FixedClock))
}
