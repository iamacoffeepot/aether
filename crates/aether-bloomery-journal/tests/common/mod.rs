//! Shared fixed clock and batch helper for journal tests.

use aether_bloomery_journal::{Batch, Clock, Draft};

/// Clock that always returns the milliseconds it was constructed with.
pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.0
    }
}

/// Wrap already-encoded drafts in a [`Batch`], preserving order.
pub fn batch_from_drafts(drafts: impl IntoIterator<Item = Draft>) -> Batch {
    let mut batch = Batch::new();
    for draft in drafts {
        batch.push_draft(draft);
    }
    batch
}
