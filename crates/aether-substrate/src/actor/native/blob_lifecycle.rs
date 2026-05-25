//! Packed lifecycle word for the cursor-shared cooperative blob
//! (iamacoffeepot/aether#1137). One `AtomicU64` coordinates a single
//! producer publishing recipient-groups and many workers cooperatively
//! draining them:
//!
//! ```text
//! bit   63        42..62       21..41       0..20
//!     [seal:1]  [ done:21 ]  [ len:21 ]  [ cursor:21 ]
//! ```
//!
//! - `cursor` — index of the next group a worker will claim (monotonic;
//!   only ever increases).
//! - `len`    — number of groups published so far. **Single-writer**: only
//!   the producing actor's thread bumps it ([`Lifecycle::publish`]).
//! - `done`   — number of groups a worker has finished draining.
//! - `seal`   — set by the worker whose `complete` brings `done == len`
//!   (which implies `cursor == len`, all groups claimed and drained): the
//!   blob is retired and accepts no further appends.
//!
//! Each op is a CAS on the whole word and retries on contention. `len`
//! moves only under the single producer, so a [`Lifecycle::publish`] CAS
//! only ever loses to a concurrent `claim` (cursor) or `complete` (done)
//! and retries with the same intended `len + count`.
//!
//! ## Publication ordering
//!
//! The producer writes a new group into the shared array **before**
//! calling [`Lifecycle::publish`]; `publish`'s `AcqRel` CAS release-stores
//! the bumped `len`, and a worker's [`Lifecycle::claim`] `Acquire`-loads
//! the word. A worker that claims index `i` observed `len > i`, so it
//! synchronizes-with the `publish` that bumped `len` past `i` and sees the
//! group write that preceded it.

// Every `u64 -> usize` cast below is a packed field bounded by
// `FIELD_MASK` (`< 2^21`), which fits `usize` on every target aether
// builds for (wasm32 / x86-64 / aarch64, all >= 32-bit), so the truncation
// the lint warns about cannot occur.
#![allow(
    clippy::cast_possible_truncation,
    reason = "packed fields are < 2^21, fit usize on all supported targets"
)]

use std::sync::atomic::{AtomicU64, Ordering};

/// Bits per packed field (`cursor` / `len` / `done`). Three fields plus
/// the seal bit fill the 64-bit word: `3 * 21 + 1 == 64`.
const FIELD_BITS: u32 = 21;
const FIELD_MASK: u64 = (1 << FIELD_BITS) - 1;
const LEN_SHIFT: u32 = FIELD_BITS;
const DONE_SHIFT: u32 = 2 * FIELD_BITS;
const SEAL_BIT: u64 = 1 << 63;

/// Max groups a single blob can hold — the 21-bit `len` ceiling. A blob
/// whose producer would publish past this seals and the producer rolls a
/// fresh blob for the remainder. The shared group array is sized far
/// below this in practice (to the flush width); this is the hard wire
/// ceiling, not the typical cap.
pub const MAX_GROUPS: usize = FIELD_MASK as usize;

#[inline]
fn cursor_of(w: u64) -> u64 {
    w & FIELD_MASK
}
#[inline]
fn len_of(w: u64) -> u64 {
    (w >> LEN_SHIFT) & FIELD_MASK
}
#[inline]
fn done_of(w: u64) -> u64 {
    (w >> DONE_SHIFT) & FIELD_MASK
}
#[inline]
fn sealed_of(w: u64) -> bool {
    w & SEAL_BIT != 0
}
#[inline]
fn pack(cursor: u64, len: u64, done: u64, seal: bool) -> u64 {
    cursor | (len << LEN_SHIFT) | (done << DONE_SHIFT) | if seal { SEAL_BIT } else { 0 }
}

/// Outcome of a producer's [`Lifecycle::publish`].
#[derive(Debug, PartialEq, Eq)]
pub enum Published {
    /// Groups published; `len` advanced by the requested count.
    Ok,
    /// The blob already sealed (a worker retired it). The producer must
    /// roll a fresh blob for these groups.
    Retired,
    /// Publishing would exceed [`MAX_GROUPS`]. The producer must roll a
    /// fresh blob.
    Full,
}

/// The packed lifecycle word. See the module doc for the bit layout and
/// the single-producer / many-worker contract.
pub struct Lifecycle {
    word: AtomicU64,
}

impl Lifecycle {
    /// A fresh blob with `initial_len` groups already published (the first
    /// flush's groups, written into the array before construction).
    pub fn new(initial_len: usize) -> Self {
        debug_assert!(
            initial_len <= MAX_GROUPS,
            "initial_len exceeds field ceiling"
        );
        Self {
            word: AtomicU64::new(pack(0, initial_len as u64, 0, false)),
        }
    }

    /// Worker: claim the next group index, or `None` if the cursor has
    /// caught up to `len` (nothing left to drain right now). A claimed
    /// index is handed to exactly one worker — the monotonic cursor CAS is
    /// the exactly-once gate for groups.
    pub fn claim(&self) -> Option<usize> {
        let mut w = self.word.load(Ordering::Acquire);
        loop {
            let (c, l) = (cursor_of(w), len_of(w));
            if c >= l {
                return None;
            }
            let next = pack(c + 1, l, done_of(w), sealed_of(w));
            match self
                .word
                .compare_exchange_weak(w, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(c as usize),
                Err(actual) => w = actual,
            }
        }
    }

    /// Producer (single writer): the index the next published group will
    /// occupy. The producer writes the group(s) into the shared array at
    /// `peek_len()..peek_len()+count` and then calls [`Self::publish`].
    /// Valid as a plain load because `len` has a single writer.
    pub fn peek_len(&self) -> usize {
        len_of(self.word.load(Ordering::Acquire)) as usize
    }

    /// Producer (single writer): publish `count` newly-written groups by
    /// advancing `len`. Rejects with [`Published::Retired`] if a worker
    /// already sealed the blob, or [`Published::Full`] if the bump would
    /// exceed [`MAX_GROUPS`]; in both cases the producer rolls a fresh
    /// blob. The `AcqRel` CAS release-stores the new `len`, publishing the
    /// array writes that preceded this call.
    pub fn publish(&self, count: usize) -> Published {
        if count == 0 {
            return Published::Ok;
        }
        let mut w = self.word.load(Ordering::Acquire);
        loop {
            if sealed_of(w) {
                return Published::Retired;
            }
            let l = len_of(w);
            if l as usize + count > MAX_GROUPS {
                return Published::Full;
            }
            let next = pack(cursor_of(w), l + count as u64, done_of(w), false);
            match self
                .word
                .compare_exchange_weak(w, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Published::Ok,
                Err(actual) => w = actual,
            }
        }
    }

    /// Worker: mark one claimed group finished. Sets the seal bit when this
    /// completion brings `done == len` (all groups drained), retiring the
    /// blob. Returns `true` if this call retired the blob.
    pub fn complete(&self) -> bool {
        let mut w = self.word.load(Ordering::Acquire);
        loop {
            let new_done = done_of(w) + 1;
            let retire = new_done == len_of(w);
            let next = pack(cursor_of(w), len_of(w), new_done, sealed_of(w) || retire);
            match self
                .word
                .compare_exchange_weak(w, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return retire,
                Err(actual) => w = actual,
            }
        }
    }

    /// `true` once the blob has sealed (retired). The producer reads this
    /// to decide whether to append to this blob or roll a fresh one.
    pub fn is_retired(&self) -> bool {
        sealed_of(self.word.load(Ordering::Acquire))
    }

    /// `(cursor, len, done, sealed)` snapshot. Tests + diagnostics only;
    /// production scheduling goes through the ops above.
    #[cfg(test)]
    pub fn snapshot(&self) -> (usize, usize, usize, bool) {
        let w = self.word.load(Ordering::Acquire);
        (
            cursor_of(w) as usize,
            len_of(w) as usize,
            done_of(w) as usize,
            sealed_of(w),
        )
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::needless_collect,
    reason = "test setup: unwraps signal failure; thread handles are collected so every worker is spawned before any join"
)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn new_starts_at_zero_cursor_unsealed() {
        let lc = Lifecycle::new(4);
        assert_eq!(lc.snapshot(), (0, 4, 0, false));
        assert_eq!(lc.peek_len(), 4);
        assert!(!lc.is_retired());
    }

    #[test]
    fn claim_hands_out_each_index_once_then_drains() {
        let lc = Lifecycle::new(3);
        assert_eq!(lc.claim(), Some(0));
        assert_eq!(lc.claim(), Some(1));
        assert_eq!(lc.claim(), Some(2));
        assert_eq!(lc.claim(), None, "cursor caught up to len");
        assert_eq!(lc.claim(), None, "stays drained");
    }

    #[test]
    fn publish_extends_claimable_range() {
        let lc = Lifecycle::new(1);
        assert_eq!(lc.claim(), Some(0));
        assert_eq!(lc.claim(), None);
        assert_eq!(lc.peek_len(), 1);
        assert_eq!(lc.publish(2), Published::Ok);
        assert_eq!(lc.snapshot().1, 3, "len advanced by 2");
        assert_eq!(lc.claim(), Some(1), "the appended groups are now claimable");
        assert_eq!(lc.claim(), Some(2));
        assert_eq!(lc.claim(), None);
    }

    #[test]
    fn complete_sets_seal_when_done_reaches_len() {
        let lc = Lifecycle::new(2);
        assert_eq!(lc.claim(), Some(0));
        assert_eq!(lc.claim(), Some(1));
        assert!(!lc.complete(), "first completion does not retire");
        assert!(!lc.is_retired());
        assert!(lc.complete(), "completion bringing done==len retires");
        assert!(lc.is_retired());
        assert_eq!(lc.snapshot(), (2, 2, 2, true));
    }

    #[test]
    fn publish_after_seal_is_rejected() {
        let lc = Lifecycle::new(1);
        let _ = lc.claim();
        assert!(lc.complete(), "single group completes -> retire");
        assert_eq!(
            lc.publish(1),
            Published::Retired,
            "no append onto a retired blob"
        );
    }

    #[test]
    fn publish_past_ceiling_is_full() {
        let lc = Lifecycle::new(MAX_GROUPS);
        assert_eq!(lc.publish(1), Published::Full);
    }

    #[test]
    fn publish_zero_is_noop_ok() {
        let lc = Lifecycle::new(2);
        assert_eq!(lc.publish(0), Published::Ok);
        assert_eq!(lc.snapshot().1, 2);
    }

    /// Concurrent claimers hand out every index in `0..len` exactly once,
    /// none twice, none skipped — the cursor CAS is the exactly-once gate.
    #[test]
    fn concurrent_claims_partition_the_range() {
        const LEN: usize = 4096;
        let lc = Arc::new(Lifecycle::new(LEN));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let lc = Arc::clone(&lc);
                thread::spawn(move || {
                    let mut got = Vec::new();
                    while let Some(i) = lc.claim() {
                        got.push(i);
                    }
                    got
                })
            })
            .collect();
        let mut all: Vec<usize> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        all.sort_unstable();
        let unique: BTreeSet<usize> = all.iter().copied().collect();
        assert_eq!(all.len(), LEN, "every claim accounted for, no duplicates");
        assert_eq!(unique.len(), LEN, "no index handed out twice");
        assert_eq!(*unique.iter().next().unwrap(), 0);
        assert_eq!(*unique.iter().next_back().unwrap(), LEN - 1);
    }

    /// A single producer publishing concurrently with many claimers never
    /// loses or duplicates a group index: the published range `0..len` is
    /// partitioned across the claimers exactly once. (Completion / retire
    /// is exercised single-threaded above and end-to-end by the blob's own
    /// concurrency tests — this isolates the publish-vs-claim race, so the
    /// workers here don't `complete` and the blob never seals mid-stream.)
    #[test]
    fn producer_publishes_while_workers_claim() {
        use std::sync::Mutex;
        use std::sync::atomic::AtomicBool;
        const BATCHES: usize = 2000;
        let lc = Arc::new(Lifecycle::new(1));
        let producer_done = Arc::new(AtomicBool::new(false));
        let claimed: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));

        let producer = {
            let lc = Arc::clone(&lc);
            let producer_done = Arc::clone(&producer_done);
            thread::spawn(move || {
                for _ in 0..BATCHES {
                    assert_eq!(
                        lc.publish(1),
                        Published::Ok,
                        "no completer -> never retires"
                    );
                }
                producer_done.store(true, Ordering::Release);
            })
        };
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let lc = Arc::clone(&lc);
                let producer_done = Arc::clone(&producer_done);
                let claimed = Arc::clone(&claimed);
                thread::spawn(move || {
                    let mut mine = Vec::new();
                    loop {
                        if let Some(i) = lc.claim() {
                            mine.push(i);
                        } else if producer_done.load(Ordering::Acquire) && lc.claim().is_none() {
                            break;
                        } else {
                            thread::yield_now();
                        }
                    }
                    claimed.lock().unwrap().extend(mine);
                })
            })
            .collect();

        producer.join().unwrap();
        for w in workers {
            w.join().unwrap();
        }
        let mut all = Arc::try_unwrap(claimed).unwrap().into_inner().unwrap();
        all.sort_unstable();
        let unique: BTreeSet<usize> = all.iter().copied().collect();
        assert_eq!(
            all.len(),
            1 + BATCHES,
            "every published group claimed exactly once"
        );
        assert_eq!(unique.len(), 1 + BATCHES, "no index handed out twice");
        assert_eq!(*unique.iter().next_back().unwrap(), BATCHES);
    }
}
