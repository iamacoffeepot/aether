//! A slab: one allocation whose regions become separate entries, for a
//! producer whose members live and die together (ADR-0238 decision 8).
//!
//! [`SlabBuilder`] allocates the whole slab once at the exact sum of the
//! declared lengths and hands the caller one region per length to fill in
//! place. The regions exist only as the builder hands them out, so none can
//! overlap or run out of bounds. [`SlabBuilder::finish`] interns each region
//! as its own [`BlobEntry`], with its own hash and dedup slot.
//!
//! The slab stays resident while any of its entries lives, and it is counted
//! in `resident_bytes` and `slab_bytes` from allocation until it drops.

use std::mem;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::entry::{self, BlobEntry};
use super::{Shared, gauge, reclaim, resident};

/// The one buffer a slab's entries share. Its drop frees the buffer, through
/// the reclaim thread when it is large, and removes it from the counts. It
/// never takes the index lock.
pub(super) struct Slab {
    bytes: Box<[u8]>,
    home: Arc<Shared>,
}

impl Slab {
    /// A zeroed slab of `len` bytes, counted as resident from here.
    fn new(len: usize, home: Arc<Shared>) -> Self {
        let bytes = vec![0; len].into_boxed_slice();
        home.slab_bytes.fetch_add(len, Ordering::Relaxed);
        let resident = home.resident_bytes.fetch_add(len, Ordering::Relaxed) + len;
        gauge::observe(&home, resident);
        Self { bytes, home }
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Slab {
    fn drop(&mut self) {
        let bytes = mem::take(&mut self.bytes);
        self.home.slab_bytes.fetch_sub(bytes.len(), Ordering::Relaxed);
        self.home.resident_bytes.fetch_sub(bytes.len(), Ordering::Relaxed);

        reclaim::route(&self.home.reclaim, bytes);
    }
}

/// Builds the entries of one slab: fill every region, then [`finish`].
/// Dropping it unfinished frees the slab and interns nothing.
///
/// [`finish`]: SlabBuilder::finish
pub struct SlabBuilder {
    slab: Slab,
    ranges: Vec<Range<usize>>,
}

impl SlabBuilder {
    /// One zeroed slab of exactly the sum of `lens`, split into one region
    /// per length, back to back.
    ///
    /// # Panics
    ///
    /// As `vec!` does, when the total cannot be allocated.
    pub(super) fn new(lens: &[usize], home: Arc<Shared>) -> Self {
        let total = lens.iter().copied().fold(0, usize::saturating_add);
        let slab = Slab::new(total, home);
        let ranges = lens
            .iter()
            .scan(0, |start, &len| {
                let range = *start..*start + len;
                *start = range.end;
                Some(range)
            })
            .collect();
        Self { slab, ranges }
    }

    /// Each declared region, in declared order, for the caller to fill.
    pub(crate) fn regions(&mut self) -> impl Iterator<Item = &mut [u8]> {
        let mut rest: &mut [u8] = &mut self.slab.bytes;
        self.ranges.iter().map(move |range| {
            let (region, tail) = mem::take(&mut rest).split_at_mut(range.len());
            rest = tail;
            region
        })
    }

    /// Intern every region: exactly one entry per declared length, in
    /// declared order. A region whose hash is already resident returns the
    /// resident entry, in either storage form, and stays in the slab unused
    /// until the slab drops. Every other region becomes a new slab-backed
    /// entry.
    #[must_use]
    pub(crate) fn finish(self) -> Vec<Arc<BlobEntry>> {
        let Self { slab, ranges } = self;
        let hashes: Vec<_> = ranges.iter().map(|range| entry::hash_of(&slab.bytes[range.clone()])).collect();
        let slab = Arc::new(slab);
        let home = Arc::clone(&slab.home);

        let mut index = home.lock_index();
        let entries = ranges
            .into_iter()
            .zip(hashes)
            .map(|(range, hash)| {
                resident(&index, hash).unwrap_or_else(|| {
                    let entry = Arc::new(BlobEntry::slab(hash, Arc::clone(&slab), range, Arc::clone(&home)));
                    index.insert(hash, Arc::downgrade(&entry));
                    entry
                })
            })
            .collect();
        drop(index);

        entries
    }
}
