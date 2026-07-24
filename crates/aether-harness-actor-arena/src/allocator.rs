use std::{
    array::from_fn,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

/// Physical actor location. The generation makes a reused slot distinguishable
/// from a stale route endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActorCoordinate {
    pub page: u32,
    pub slot: u8,
    pub generation: u32,
}

struct PageBits {
    free: AtomicU64,
    generations: [AtomicU32; 64],
}

/// Two-level atomic availability bitmap: one summary word covers up to 64
/// pages, and each leaf word covers up to 64 slots.
///
/// The leaf CAS is authoritative. The summary is only a repairable index, so a
/// racing release cannot make capacity permanently invisible.
pub struct HierarchicalBitmap {
    summary: AtomicU64,
    pages: Vec<PageBits>,
    page_slots: usize,
    capacity: usize,
    cas_retries: AtomicU64,
}

impl HierarchicalBitmap {
    /// Construct an empty bitmap with every coordinate available.
    ///
    /// # Panics
    ///
    /// Panics when capacity is zero, the page size is not a power of two
    /// between 1 and 64, or capacity requires more than 64 leaf pages.
    #[must_use]
    pub fn new(capacity: usize, page_slots: usize) -> Self {
        assert!(capacity > 0, "capacity must be non-zero");
        assert!(
            page_slots > 0 && page_slots <= 64 && page_slots.is_power_of_two(),
            "page_slots must be a power of two in 1..=64"
        );

        let page_count = capacity.div_ceil(page_slots);
        assert!(page_count <= 64, "the spike's two-level bitmap supports at most 64 pages");

        let pages = (0..page_count)
            .map(|page| {
                let remaining = capacity.saturating_sub(page * page_slots);
                let live_slots = remaining.min(page_slots);
                let free = if live_slots == 64 {
                    u64::MAX
                } else {
                    (1_u64 << live_slots) - 1
                };

                PageBits { free: AtomicU64::new(free), generations: from_fn(|_| AtomicU32::new(1)) }
            })
            .collect();
        let summary = if page_count == 64 {
            u64::MAX
        } else {
            (1_u64 << page_count) - 1
        };

        Self { summary: AtomicU64::new(summary), pages, page_slots, capacity, cas_retries: AtomicU64::new(0) }
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    pub fn reserve(&self) -> Option<ActorCoordinate> {
        loop {
            let summary = self.summary.load(Ordering::Acquire);
            if summary == 0 {
                return None;
            }

            let page_index = summary.trailing_zeros() as usize;
            let page_bit = 1_u64 << page_index;
            let page = &self.pages[page_index];
            let free = page.free.load(Ordering::Acquire);

            if free == 0 {
                self.clear_and_repair_summary(page_index, page_bit);
                continue;
            }

            let slot = free.trailing_zeros() as usize;
            let slot_bit = 1_u64 << slot;
            match page.free.compare_exchange_weak(free, free & !slot_bit, Ordering::AcqRel, Ordering::Acquire) {
                Ok(next) => {
                    if next & !slot_bit == 0 {
                        self.clear_and_repair_summary(page_index, page_bit);
                    }

                    return Some(ActorCoordinate {
                        page: u32::try_from(page_index).ok()?,
                        slot: u8::try_from(slot).ok()?,
                        generation: page.generations[slot].load(Ordering::Acquire),
                    });
                }
                Err(_) => {
                    self.cas_retries.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Return `false` for a stale coordinate or a double release.
    pub fn release(&self, coordinate: ActorCoordinate) -> bool {
        let page_index = coordinate.page as usize;
        let slot = coordinate.slot as usize;
        let Some(page) = self.pages.get(page_index) else {
            return false;
        };
        if slot >= self.page_slots || page_index * self.page_slots + slot >= self.capacity {
            return false;
        }

        let generation = &page.generations[slot];
        if generation
            .compare_exchange(
                coordinate.generation,
                coordinate.generation.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }

        let slot_bit = 1_u64 << slot;
        if page.free.fetch_or(slot_bit, Ordering::Release) & slot_bit != 0 {
            generation.store(coordinate.generation, Ordering::Release);
            return false;
        }
        self.summary.fetch_or(1_u64 << page_index, Ordering::Release);

        true
    }

    #[must_use]
    pub fn is_live(&self, coordinate: ActorCoordinate) -> bool {
        let page_index = coordinate.page as usize;
        let slot = coordinate.slot as usize;
        let Some(page) = self.pages.get(page_index) else {
            return false;
        };
        if slot >= self.page_slots || page_index * self.page_slots + slot >= self.capacity {
            return false;
        }

        let slot_bit = 1_u64 << slot;
        page.free.load(Ordering::Acquire) & slot_bit == 0
            && page.generations[slot].load(Ordering::Acquire) == coordinate.generation
    }

    #[must_use]
    pub fn cas_retries(&self) -> u64 {
        self.cas_retries.load(Ordering::Relaxed)
    }

    fn clear_and_repair_summary(&self, page_index: usize, page_bit: u64) {
        self.summary.fetch_and(!page_bit, Ordering::AcqRel);
        if self.pages[page_index].free.load(Ordering::Acquire) != 0 {
            self.summary.fetch_or(page_bit, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex},
        thread,
    };

    use super::HierarchicalBitmap;

    #[test]
    fn fills_reuses_and_rejects_stale_coordinate() {
        let bitmap = HierarchicalBitmap::new(130, 64);
        let coordinates: Vec<_> = (0..130).map(|_| bitmap.reserve().expect("free slot")).collect();
        let unique: HashSet<_> = coordinates.iter().copied().collect();

        assert_eq!(unique.len(), 130);
        assert!(bitmap.reserve().is_none());

        let stale = coordinates[72];
        assert!(bitmap.is_live(stale));
        assert!(bitmap.release(stale));
        assert!(!bitmap.is_live(stale));
        assert!(!bitmap.release(stale));

        let replacement = bitmap.reserve().expect("released slot");
        assert_eq!((replacement.page, replacement.slot), (stale.page, stale.slot));
        assert_ne!(replacement.generation, stale.generation);
        assert!(bitmap.is_live(replacement));
    }

    #[test]
    fn concurrent_reservations_are_unique() {
        let bitmap = Arc::new(HierarchicalBitmap::new(4_096, 64));
        let reserved = Arc::new(Mutex::new(Vec::new()));

        thread::scope(|scope| {
            for _ in 0..8 {
                let bitmap = Arc::clone(&bitmap);
                let reserved = Arc::clone(&reserved);
                scope.spawn(move || {
                    while let Some(coordinate) = bitmap.reserve() {
                        reserved.lock().expect("reservation collector").push(coordinate);
                    }
                });
            }
        });

        let reserved = reserved.lock().expect("reservation collector");
        assert_eq!(reserved.len(), bitmap.capacity());
        assert_eq!(reserved.iter().copied().collect::<HashSet<_>>().len(), bitmap.capacity());
        drop(reserved);
    }
}
