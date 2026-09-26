//! The resident-byte gauge: one warning per new high-water mark, after the
//! wasm `ReplyTable::high_water` rule. The first mark is
//! `RESIDENT_WARNING_START_BYTES`, and each crossing doubles it until it is at
//! or above the resident total. Each warning also reports the slab bytes and
//! live slab-member bytes, so what slabs retain is visible beside the total.

use std::sync::atomic::Ordering;

use super::Shared;

/// The mark after `mark` once `resident` bytes are resident, or `None` while
/// `resident` has not crossed `mark`.
pub(super) fn next_mark(mark: usize, resident: usize) -> Option<usize> {
    if resident <= mark {
        return None;
    }
    let mut next = mark;
    while next < resident {
        next = next.saturating_mul(2).max(1);
    }
    Some(next)
}

/// Advance `home`'s next warning mark past `resident`, warning once when this
/// call is the one that moves it. Check-ins that cross one mark concurrently
/// race on the compare-exchange, so exactly one of them warns.
pub(super) fn observe(home: &Shared, resident: usize) {
    let next_warning = &home.next_warning_bytes;
    let mut mark = next_warning.load(Ordering::Relaxed);
    while let Some(next) = next_mark(mark, resident) {
        match next_warning.compare_exchange_weak(mark, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                tracing::warn!(
                    resident_bytes = resident,
                    slab_bytes = home.slab_bytes.load(Ordering::Relaxed),
                    slab_member_bytes = home.slab_member_bytes.load(Ordering::Relaxed),
                    next_warning_bytes = next,
                    "blob store resident bytes reached a new high-water mark"
                );
                return;
            }
            Err(current) => mark = current,
        }
    }
}
