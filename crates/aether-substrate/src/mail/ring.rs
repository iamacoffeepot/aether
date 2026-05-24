//! `MailRing` — a single-producer, multi-consumer reclaiming byte ring
//! for blob-of-mail dispatch (ADR-0087, Phase 2 / iamacoffeepot/aether#1105).
//!
//! Phase 2a builds and unit-tests the mechanism in isolation; nothing on
//! the live dispatch path constructs `MailRef::InRing` yet (that is the
//! 2b integration). The ring is the substrate behind the blob axiom: a
//! handler's outbound mail is buffered into one contiguous region (the
//! blob), and recipients receive a [`MailRef::InRing`] ref into it
//! instead of an owned `Vec<u8>` copy.
//!
//! # Distinct from the trace rings
//!
//! The per-actor trace rings (ADR-0081, `aether_actor::trace_ring`) are
//! loss-tolerant: they overwrite the oldest entry when full. This ring
//! is **no-loss and reclaiming** — a region stays live until every
//! consumer that holds a ref into it has released, and the producer
//! never overwrites a live region.
//!
//! # Concurrency model (the Disruptor single-writer discipline)
//!
//! - **One producer** (the owning actor's thread, or the chassis for
//!   off-actor producers). It alone calls [`MailRing::push_blob`] and
//!   [`MailRing::reclaim`], and is the sole writer of the `write` /
//!   `front` cursors and of the buffer bytes.
//! - **Many consumers** (worker threads). Per blob they read payload
//!   bytes ([`MailRing::payload`]) and decrement the blob's lock
//!   ([`MailRing::release`]). Consumers never touch the cursors or write
//!   bytes.
//! - **The reclaim invariant.** The producer writes only into free space
//!   (past every live blob) and advances `front` past a blob only once
//!   its `lock == 0`. `lock == 0` is observed with `Acquire`, which
//!   synchronizes-with every consumer's `Release` decrement, so all
//!   consumer reads of that region happened-before the producer reuses
//!   it. Hence no data race on payload bytes despite the `UnsafeCell`
//!   backing — this is what makes decode-in-place sound.
//!
//! The blob lock is initialized to the mail count *before* the refs are
//! handed to consumers; that handoff goes through the recipient inbox
//! channel (in 2b), which provides the happens-before edge that
//! publishes the initialized lock + payload bytes to the consumer.

use std::alloc::{Layout, alloc, dealloc};
use std::cell::UnsafeCell;
use std::fmt;
use std::slice;
use std::sync::atomic::{AtomicU32, Ordering};

/// Backing-buffer alignment. `BlobHeader` and `MailEntry` are 8-aligned
/// (`u64` fields), and every sub-record is padded to a multiple of 8, so
/// an 8-aligned base keeps every in-place cast aligned.
const ALIGN: usize = 8;

/// Round `n` up to the next multiple of [`ALIGN`].
#[inline]
const fn align_up(n: usize) -> usize {
    (n + ALIGN - 1) & !(ALIGN - 1)
}

/// Per-blob header, written at the start of each blob region. `lock` is
/// the live-reference count: initialized to the blob's mail count, each
/// consumer decrements by one (or the inline-drain path batch-decrements),
/// and the producer reclaims the region once it reads zero.
///
/// `#[repr(C, align(8))]`, 16 bytes. The `AtomicU32` makes the struct
/// non-`Pod`, so it is read back through a raw `*const BlobHeader` cast
/// rather than `bytemuck` (the other fields are producer-written and
/// producer-read, so they need no atomicity).
#[repr(C, align(8))]
struct BlobHeader {
    lock: AtomicU32,
    n_mails: u32,
    /// Total bytes of this blob region (header + entries + padded
    /// payloads), so reclaim can stride to the next blob without
    /// re-walking the entries.
    total_len: u32,
    /// `1` marks a wrap filler: padding written when a blob would not fit
    /// in `[write, cap)`, telling reclaim to jump `front` to `0`. Fillers
    /// carry `lock == 0` so they reclaim immediately. `0` for real blobs.
    is_filler: u32,
}

const HEADER_LEN: usize = size_of::<BlobHeader>();

/// One mail's metadata inside a blob, written immediately before its
/// payload. `#[repr(C)]`, 24 bytes, 8-aligned — `bytemuck`-castable
/// (all-integer, no atomics).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MailEntry {
    /// Payload byte length (unpadded).
    len: u32,
    _pad: u32,
    recipient: u64,
    kind: u64,
}

const ENTRY_LEN: usize = size_of::<MailEntry>();

/// One mail to write into a blob: the route metadata plus a borrow of
/// the payload bytes the producer copies into the ring.
#[derive(Clone, Copy)]
pub struct OutMail<'a> {
    pub recipient: u64,
    pub kind: u64,
    pub payload: &'a [u8],
}

/// Where one written mail landed in the ring: enough for the caller (2b)
/// to mint a [`MailRef::InRing`] and route it. `header_off` locates the
/// blob's [`BlobHeader`] for [`MailRing::release`]; `payload_off` /
/// `len` bound the payload for [`MailRing::payload`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailLoc {
    pub recipient: u64,
    pub kind: u64,
    pub header_off: u32,
    pub payload_off: u32,
    pub len: u32,
}

/// Returned by [`MailRing::push_blob`] when the blob does not fit in the
/// free space even after a wrap. The caller's never-block-the-producer
/// valve copies the mails out to owned buffers instead (the cost is a
/// memcpy — the same allocation today's eager envelopes pay).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingFull;

/// A single-producer, multi-consumer reclaiming byte ring. See the
/// module docs for the concurrency model.
pub struct MailRing {
    /// 8-aligned backing buffer, `cap` bytes. Wrapped in `UnsafeCell`
    /// because the producer writes free regions while consumers read
    /// live regions; the reclaim invariant keeps those regions disjoint.
    buf: *mut u8,
    cap: usize,
    /// Producer write cursor (byte offset of the next free slot). Only
    /// the producer stores; atomic purely so `MailRing: Sync`.
    write: AtomicU32,
    /// Producer reclaim cursor (byte offset of the oldest live blob).
    /// Only the producer stores.
    front: AtomicU32,
    /// Live bytes currently occupied by un-reclaimed blobs, tracked so
    /// free-space math is a single subtraction rather than a cursor
    /// comparison that has to disambiguate full-vs-empty.
    live: UnsafeCell<usize>,
    _backing: BufOwner,
}

/// Owns the raw allocation so [`MailRing`]'s `Drop` frees it with the
/// matching `Layout`. Split out so the unsafe alloc/dealloc pairing sits
/// in one place.
struct BufOwner {
    ptr: *mut u8,
    layout: Layout,
}

impl Drop for BufOwner {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `alloc(layout)` in `MailRing::with_capacity`
        // and is freed exactly once (on `MailRing` drop).
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

#[allow(
    clippy::non_send_fields_in_send_ty,
    reason = "the raw buffer pointer is shared under the documented single-producer discipline"
)]
// SAFETY: the producer/consumer split (single writer of cursors + free
// region; consumers read live regions + atomic lock only) makes the raw
// `*mut u8` safe to send across threads (see the module concurrency model);
// the pointer is owned for the ring's lifetime by `_backing`.
unsafe impl Send for MailRing {}
// SAFETY: same discipline — concurrent access is producer writes to free
// space + consumer reads of live regions + atomic lock ops, which the
// reclaim invariant keeps from overlapping.
unsafe impl Sync for MailRing {}

impl fmt::Debug for MailRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailRing")
            .field("cap", &self.cap)
            .field("live_bytes", &self.live_bytes())
            .field("write", &self.write.load(Ordering::Relaxed))
            .field("front", &self.front.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl MailRing {
    /// Allocate a ring with `cap` bytes (rounded up to [`ALIGN`]). `cap`
    /// must be non-zero.
    ///
    /// # Panics
    /// Panics if `cap == 0` or the allocation fails.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        assert!(cap > 0, "MailRing capacity must be non-zero");
        let cap = align_up(cap);
        // Offsets and lengths are stored as `u32` (in `MailLoc`, the blob
        // header, and the cursors), so the buffer must fit in `u32`.
        assert!(
            u32::try_from(cap).is_ok(),
            "MailRing capacity ({cap} B) must fit u32"
        );
        let layout = Layout::from_size_align(cap, ALIGN).expect("valid ring layout");
        // SAFETY: `cap > 0` so the layout is non-zero-sized; we check the
        // returned pointer for null below.
        let ptr = unsafe { alloc(layout) };
        assert!(!ptr.is_null(), "MailRing allocation failed");
        Self {
            buf: ptr,
            cap,
            write: AtomicU32::new(0),
            front: AtomicU32::new(0),
            live: UnsafeCell::new(0),
            _backing: BufOwner { ptr, layout },
        }
    }

    /// Total capacity in bytes.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Bytes currently occupied by live (un-reclaimed) blobs. Producer-only
    /// (reads the producer-owned `live` counter).
    #[must_use]
    pub fn live_bytes(&self) -> usize {
        // SAFETY: `live` is only ever touched by the producer thread, which
        // is the sole caller of this and the mutating methods.
        unsafe { *self.live.get() }
    }

    /// Bytes of contiguous-or-wrapped free space the producer could use.
    fn free_bytes(&self) -> usize {
        self.cap - self.live_bytes()
    }

    /// Size of a blob region for `mails`: header + per-mail (entry +
    /// padded payload), the whole thing padded to [`ALIGN`].
    fn blob_size(mails: &[OutMail<'_>]) -> usize {
        let mut n = HEADER_LEN;
        for m in mails {
            n += ENTRY_LEN + align_up(m.payload.len());
        }
        align_up(n)
    }

    /// Raw pointer to the blob header at byte offset `off`. Centralizes
    /// the `*mut u8 -> *mut BlobHeader` cast (and its alignment `allow`).
    ///
    /// # Safety
    /// `off` must be an in-bounds, [`ALIGN`]-aligned header offset within
    /// the buffer (every blob/filler starts on an `ALIGN` boundary, so a
    /// `header_off` / `front` value always satisfies this).
    #[allow(
        clippy::cast_ptr_alignment,
        reason = "buffer base + every blob offset are ALIGN(8)-aligned by construction, so the BlobHeader cast is aligned"
    )]
    unsafe fn header_ptr(&self, off: usize) -> *mut BlobHeader {
        // SAFETY: caller guarantees `off` is an in-bounds aligned header offset.
        unsafe { self.buf.add(off).cast::<BlobHeader>() }
    }

    /// Write a blob of `mails` into the ring as one contiguous region and
    /// return where each mail landed. Initializes the blob lock to
    /// `mails.len()`. **Producer-only.**
    ///
    /// Returns [`RingFull`] if the blob does not fit even after a wrap;
    /// the caller's copy-out valve handles that without blocking.
    ///
    /// # Panics
    /// Panics if `mails` is empty (a blob always carries at least one
    /// mail) or if a single blob is larger than the whole ring (a
    /// misconfiguration — the ring must be sized for the largest blob).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "offsets are bounded by capacity, asserted <= u32::MAX in with_capacity"
    )]
    pub fn push_blob(&self, mails: &[OutMail<'_>]) -> Result<Vec<MailLoc>, RingFull> {
        assert!(!mails.is_empty(), "push_blob requires at least one mail");
        let size = Self::blob_size(mails);
        assert!(
            size <= self.cap,
            "blob ({size} B) larger than ring capacity ({} B)",
            self.cap
        );

        let write = self.write.load(Ordering::Relaxed) as usize;
        let tail_room = self.cap - write;

        // Choose the start offset, accounting for wrap. A blob never
        // straddles the end of the buffer, so if it does not fit in the
        // tail we lay a filler over `[write, cap)` and start at 0.
        let start = if size <= tail_room {
            write
        } else {
            // Need a filler over the tail plus the blob at 0. Both must
            // fit in current free space.
            if tail_room + size > self.free_bytes() {
                return Err(RingFull);
            }
            self.write_filler(write, tail_room);
            0
        };

        // After a possible filler, re-check the blob itself fits the
        // remaining free space.
        if size > self.free_bytes() {
            return Err(RingFull);
        }

        // SAFETY: `start..start+size` lies in free space (past every live
        // blob), so no consumer can be reading it; the producer is the
        // sole writer. Alignment holds because `start` is always a
        // multiple of ALIGN (cursor only ever advances by aligned sizes).
        let locs = unsafe { self.write_blob_at(start, mails, size) };

        let new_write = if start + size == self.cap {
            0
        } else {
            start + size
        };
        self.write.store(new_write as u32, Ordering::Relaxed);
        // SAFETY: producer-only mutation of the live counter.
        unsafe {
            *self.live.get() += size;
            if start == 0 && write != 0 {
                // We wrapped: the filler bytes over the old tail are now
                // live too (reclaimed lazily like any other blob).
                *self.live.get() += tail_room;
            }
        }
        Ok(locs)
    }

    /// Lay a filler blob over `[off, off+len)` so reclaim knows to wrap
    /// `front` to 0 when it reaches it. Filler carries `lock == 0`.
    ///
    /// # Safety
    /// `off..off+len` must be free space the producer owns.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "len is bounded by capacity, asserted <= u32::MAX in with_capacity"
    )]
    fn write_filler(&self, off: usize, len: usize) {
        // SAFETY: caller guarantees the range is producer-owned free space.
        // The backing bytes are uninitialized, so every field is written
        // with a non-dropping `ptr::write`.
        unsafe {
            let hdr = self.header_ptr(off);
            (&raw mut (*hdr).lock).write(AtomicU32::new(0));
            (&raw mut (*hdr).n_mails).write(0);
            (&raw mut (*hdr).total_len).write(len as u32);
            (&raw mut (*hdr).is_filler).write(1);
        }
    }

    /// Write the header, entries, and payloads of `mails` at `start`.
    ///
    /// # Safety
    /// `start..start+size` must be producer-owned free space, `size`
    /// must equal [`Self::blob_size`] for `mails`, and `start` must be
    /// [`ALIGN`]-aligned.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "offsets and payload lengths are bounded by capacity, asserted <= u32::MAX in with_capacity"
    )]
    unsafe fn write_blob_at(
        &self,
        start: usize,
        mails: &[OutMail<'_>],
        size: usize,
    ) -> Vec<MailLoc> {
        // SAFETY: the caller's contract guarantees the range is free and
        // aligned; we only write within `[start, start+size)`.
        unsafe {
            let hdr = self.header_ptr(start);
            (&raw mut (*hdr).lock).write(AtomicU32::new(mails.len() as u32));
            (&raw mut (*hdr).n_mails).write(mails.len() as u32);
            (&raw mut (*hdr).total_len).write(size as u32);
            (&raw mut (*hdr).is_filler).write(0);

            let mut locs = Vec::with_capacity(mails.len());
            let mut cur = start + HEADER_LEN;
            for m in mails {
                let entry = MailEntry {
                    len: m.payload.len() as u32,
                    _pad: 0,
                    recipient: m.recipient,
                    kind: m.kind,
                };
                // Entry is Pod; write its bytes at `cur`.
                self.buf
                    .add(cur)
                    .copy_from_nonoverlapping((&raw const entry).cast::<u8>(), ENTRY_LEN);
                let payload_off = cur + ENTRY_LEN;
                if !m.payload.is_empty() {
                    self.buf
                        .add(payload_off)
                        .copy_from_nonoverlapping(m.payload.as_ptr(), m.payload.len());
                }
                locs.push(MailLoc {
                    recipient: m.recipient,
                    kind: m.kind,
                    header_off: start as u32,
                    payload_off: payload_off as u32,
                    len: m.payload.len() as u32,
                });
                cur = payload_off + align_up(m.payload.len());
            }
            locs
        }
    }

    /// Borrow a mail's payload bytes for in-place decode. **Consumer-safe**
    /// while the blob's lock is held (`> 0`): the reclaim invariant keeps
    /// the region stable until every holder releases.
    ///
    /// # Safety
    /// `off..off+len` must come from a [`MailLoc`] of a blob whose lock
    /// the caller still holds (has not yet [`released`](Self::release)).
    /// Using a stale offset after release is a use-after-reclaim.
    #[must_use]
    pub unsafe fn payload(&self, off: u32, len: u32) -> &[u8] {
        // SAFETY: caller holds the lock, so the producer cannot have
        // reclaimed/overwritten `[off, off+len)`; the bytes were published
        // through the inbox-channel handoff.
        unsafe { slice::from_raw_parts(self.buf.add(off as usize), len as usize) }
    }

    /// Decrement a blob's lock by one (a single consumer finished). When
    /// it reaches zero the region becomes reclaimable. **Consumer-safe.**
    ///
    /// # Safety
    /// `header_off` must be the `header_off` of a [`MailLoc`] whose lock
    /// the caller holds exactly one count of; releasing twice
    /// under-counts the lock and risks early reclaim.
    pub unsafe fn release(&self, header_off: u32) {
        // SAFETY: header_off addresses a live blob's header.
        let hdr = unsafe { &*self.header_ptr(header_off as usize) };
        // Release: publish this consumer's reads to the producer's Acquire
        // reclaim load.
        hdr.lock.fetch_sub(1, Ordering::Release);
    }

    /// Increment a blob's lock by one — a new holder of an `InRing` ref
    /// (a [`MailRef`](crate::mail::MailRef) clone). **Consumer-safe.**
    ///
    /// # Safety
    /// `header_off` must be the header of a blob whose lock the caller
    /// already holds at least one count of (so it cannot have been
    /// reclaimed mid-increment). `Relaxed` suffices — the held count
    /// keeps the region alive; no payload reads are being ordered here.
    pub unsafe fn acquire(&self, header_off: u32) {
        // SAFETY: header_off addresses a live blob's header (caller holds a count).
        let hdr = unsafe { &*self.header_ptr(header_off as usize) };
        hdr.lock.fetch_add(1, Ordering::Relaxed);
    }

    /// Batch-decrement a blob's lock by `n` (the inline-drain path, where
    /// the worker ran `n` of the blob's recipients itself). **Producer or
    /// consumer-safe** — it is the same atomic as [`Self::release`].
    ///
    /// # Safety
    /// Same as [`Self::release`], and `n` must not exceed the lock the
    /// caller holds.
    pub unsafe fn release_n(&self, header_off: u32, n: u32) {
        if n == 0 {
            return;
        }
        // SAFETY: header_off addresses a live blob's header.
        let hdr = unsafe { &*self.header_ptr(header_off as usize) };
        hdr.lock.fetch_sub(n, Ordering::Release);
    }

    /// Advance `front` past every fully-released (`lock == 0`) blob at the
    /// head, freeing their bytes for reuse. Lazy: the producer calls this
    /// before a write (or opportunistically). **Producer-only.** Returns
    /// the number of bytes reclaimed.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "front offset is bounded by capacity, asserted <= u32::MAX in with_capacity"
    )]
    pub fn reclaim(&self) -> usize {
        let mut reclaimed = 0;
        loop {
            // SAFETY: producer-only counter.
            if unsafe { *self.live.get() } == 0 {
                break;
            }
            let front = self.front.load(Ordering::Relaxed) as usize;
            // SAFETY: `front` addresses the oldest live blob's header.
            let hdr = unsafe { &*self.header_ptr(front) };
            // Acquire: synchronize-with consumer Release decrements so all
            // their reads happened-before we reuse the region.
            let is_filler = hdr.is_filler != 0;
            if !is_filler && hdr.lock.load(Ordering::Acquire) != 0 {
                break;
            }
            let total = hdr.total_len as usize;
            let new_front = if front + total == self.cap {
                0
            } else {
                front + total
            };
            self.front.store(new_front as u32, Ordering::Relaxed);
            // SAFETY: producer-only counter.
            unsafe {
                *self.live.get() -= total;
            }
            reclaimed += total;
        }
        reclaimed
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::undocumented_unsafe_blocks,
    clippy::cast_possible_truncation,
    clippy::needless_collect,
    reason = "test code: unwraps assert via panic; unsafe blocks exercise the ring API whose safety contracts are documented on the methods and upheld by construction here; casts are capacity-bounded; the consumer-handle collect forces every thread to spawn before any join"
)]
mod tests {
    use super::*;

    fn out(recipient: u64, kind: u64, payload: &[u8]) -> OutMail<'_> {
        OutMail {
            recipient,
            kind,
            payload,
        }
    }

    #[test]
    fn push_then_read_round_trips() {
        let ring = MailRing::with_capacity(4096);
        let locs = ring
            .push_blob(&[out(1, 10, &[1, 2, 3]), out(2, 20, &[4, 5, 6, 7])])
            .expect("fits");
        assert_eq!(locs.len(), 2);
        assert_eq!(locs[0].recipient, 1);
        assert_eq!(locs[0].kind, 10);
        assert_eq!(locs[1].recipient, 2);
        // SAFETY: locks still held (not released), region is live.
        unsafe {
            assert_eq!(ring.payload(locs[0].payload_off, locs[0].len), &[1, 2, 3]);
            assert_eq!(
                ring.payload(locs[1].payload_off, locs[1].len),
                &[4, 5, 6, 7]
            );
        }
        // both mails share one blob header
        assert_eq!(locs[0].header_off, locs[1].header_off);
    }

    #[test]
    fn reclaim_only_past_zero_lock() {
        let ring = MailRing::with_capacity(4096);
        let locs = ring
            .push_blob(&[out(1, 10, &[0; 8]), out(2, 20, &[0; 8])])
            .unwrap();
        let before = ring.live_bytes();
        assert!(before > 0);
        // one of two released → lock still 1 → no reclaim
        unsafe { ring.release(locs[0].header_off) };
        assert_eq!(ring.reclaim(), 0);
        assert_eq!(ring.live_bytes(), before);
        // second released → lock 0 → reclaim frees the blob
        unsafe { ring.release(locs[1].header_off) };
        let freed = ring.reclaim();
        assert_eq!(freed, before);
        assert_eq!(ring.live_bytes(), 0);
    }

    #[test]
    fn batch_release_reclaims() {
        let ring = MailRing::with_capacity(4096);
        let locs = ring
            .push_blob(&[out(1, 1, &[9]), out(2, 2, &[9]), out(3, 3, &[9])])
            .unwrap();
        unsafe { ring.release_n(locs[0].header_off, 3) };
        assert!(ring.reclaim() > 0);
        assert_eq!(ring.live_bytes(), 0);
    }

    #[test]
    fn wrap_lays_filler_and_starts_at_zero() {
        // Small ring; push blobs until one must wrap.
        let ring = MailRing::with_capacity(256);
        // First blob near the tail.
        let a = ring.push_blob(&[out(1, 1, &[0; 64])]).unwrap();
        let b = ring.push_blob(&[out(2, 2, &[0; 64])]).unwrap();
        // Release + reclaim the first so the front frees, making room at 0.
        unsafe { ring.release(a[0].header_off) };
        ring.reclaim();
        // This one should not fit the tail and must wrap to 0.
        let c = ring.push_blob(&[out(3, 3, &[0; 64])]).unwrap();
        assert_eq!(c[0].payload_off % ALIGN as u32, 0);
        // c wrote at the front of the buffer (offset 0 region), past the header.
        assert!(c[0].header_off < b[0].header_off);
        unsafe {
            assert_eq!(ring.payload(c[0].payload_off, c[0].len), &[0u8; 64]);
        }
    }

    #[test]
    fn full_ring_returns_ring_full() {
        let ring = MailRing::with_capacity(128);
        // Fill it; the unreleased blob keeps live bytes high.
        let _a = ring.push_blob(&[out(1, 1, &[0; 64])]).unwrap();
        // A second 64-byte payload + overhead exceeds the remaining free
        // space (nothing released), so the valve must trigger.
        let r = ring.push_blob(&[out(2, 2, &[0; 64])]);
        assert_eq!(r, Err(RingFull));
    }

    #[test]
    fn empty_payload_mail_round_trips() {
        let ring = MailRing::with_capacity(1024);
        let locs = ring.push_blob(&[out(7, 7, &[])]).unwrap();
        assert_eq!(locs[0].len, 0);
        unsafe {
            assert_eq!(ring.payload(locs[0].payload_off, locs[0].len), &[] as &[u8]);
            ring.release(locs[0].header_off);
        }
        assert!(ring.reclaim() > 0);
    }

    /// The load-bearing safety test: one producer writing + reclaiming
    /// while many consumers read payloads in place and release. If the
    /// reclaim invariant (producer reuses a region only after `lock == 0`,
    /// observed `Acquire`) were wrong, a consumer would read bytes the
    /// producer had already overwritten — caught here as a tag mismatch.
    /// Each blob's payloads are filled with a per-blob tag byte; a
    /// consumer reading a different byte means the region was reused
    /// under it.
    #[test]
    fn concurrent_producer_reclaim_and_consumers_decode_in_place() {
        use std::sync::{Arc, Mutex, mpsc};
        use std::thread;

        // A handed-off consume right: where to read, how much, the tag the
        // payload should contain, and the header to release.
        struct Ref {
            header_off: u32,
            payload_off: u32,
            len: u32,
            tag: u8,
        }

        let ring = Arc::new(MailRing::with_capacity(16 * 1024));
        let n_consumers = 4;
        let total_blobs = 20_000u32;

        let (tx, rx) = mpsc::channel::<Ref>();
        let rx = Arc::new(Mutex::new(rx));

        let consumers: Vec<_> = (0..n_consumers)
            .map(|_| {
                let ring = Arc::clone(&ring);
                let rx = Arc::clone(&rx);
                thread::spawn(move || {
                    let mut seen = 0u64;
                    loop {
                        let r = {
                            let guard = rx.lock().unwrap();
                            guard.recv()
                        };
                        let Ok(r) = r else { break };
                        // SAFETY: we hold this ref's lock count until the
                        // `release` below, so the region is live for the read.
                        let bytes = unsafe { ring.payload(r.payload_off, r.len) };
                        assert!(
                            bytes.iter().all(|&b| b == r.tag),
                            "decode-in-place saw a reused region: expected tag {}, got {:?}",
                            r.tag,
                            &bytes[..bytes.len().min(8)]
                        );
                        // SAFETY: release the single count this ref carried.
                        unsafe { ring.release(r.header_off) };
                        seen += 1;
                    }
                    seen
                })
            })
            .collect();

        // Producer: push tagged blobs, hand each mail to a consumer, and
        // reclaim as locks drain. On `RingFull`, reclaim and spin — the
        // never-block valve's backpressure analogue for the test.
        for i in 0..total_blobs {
            let tag = (i & 0xff) as u8;
            let n_mails = (i % 3 + 1) as usize;
            let payloads: Vec<Vec<u8>> = (0..n_mails).map(|k| vec![tag; 8 + k * 8]).collect();
            let mails: Vec<OutMail<'_>> = payloads
                .iter()
                .enumerate()
                .map(|(k, p)| out(k as u64, u64::from(tag), p))
                .collect();

            let locs = loop {
                if let Ok(locs) = ring.push_blob(&mails) {
                    break locs;
                }
                // Ring full: drain reclaimable blobs and retry (the test's
                // analogue of the never-block copy-out valve's backpressure).
                ring.reclaim();
                thread::yield_now();
            };
            for loc in locs {
                tx.send(Ref {
                    header_off: loc.header_off,
                    payload_off: loc.payload_off,
                    len: loc.len,
                    tag,
                })
                .unwrap();
            }
            if i % 16 == 0 {
                ring.reclaim();
            }
        }
        drop(tx);

        let consumed: u64 = consumers.into_iter().map(|h| h.join().unwrap()).sum();
        // Drain any blobs whose consumers finished after the producer's last
        // reclaim pass.
        while ring.reclaim() > 0 {}
        // mails per blob cycle (1+2+3) over total_blobs/3 cycles.
        let expected: u64 = (0..total_blobs).map(|i| u64::from(i % 3 + 1)).sum();
        assert_eq!(consumed, expected, "every handed-out mail must be consumed");
        assert_eq!(ring.live_bytes(), 0, "all blobs reclaimed once drained");
    }

    /// Producer-side microbench (run with `--ignored --nocapture`):
    /// buffering N fire-and-forget sends into one blob vs the eager path
    /// that allocates an owned `Vec` per send. Reports ns/send for both so
    /// the blob's amortization shows up. Not a CI gate — a calibration aid.
    #[test]
    #[ignore = "microbench; run with --ignored --nocapture"]
    #[allow(clippy::print_stdout, reason = "microbench reports timings to stdout")]
    fn bench_buffered_blob_vs_eager_vec() {
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        let payload = [0xABu8; 64];
        let iters = 100_000u32;
        for &n in &[1usize, 4, 16, 64] {
            let mails: Vec<OutMail<'_>> = (0..n).map(|k| out(k as u64, 1, &payload)).collect();
            // Ring sized for a few blobs of this width; reclaim between pushes.
            let ring = MailRing::with_capacity(MailRing::blob_size(&mails) * 4 + 64);

            let t0 = Instant::now();
            for _ in 0..iters {
                let locs = ring.push_blob(black_box(&mails)).unwrap();
                for loc in &locs {
                    // SAFETY: single-threaded bench; we release immediately.
                    unsafe { ring.release(loc.header_off) };
                }
                ring.reclaim();
                black_box(&locs);
            }
            let buffered = t0.elapsed();

            let t1 = Instant::now();
            for _ in 0..iters {
                let mut owned: Vec<Vec<u8>> = Vec::with_capacity(n);
                for _ in 0..n {
                    owned.push(black_box(&payload).to_vec());
                }
                black_box(&owned);
            }
            let eager = t1.elapsed();

            let sends = u64::from(iters) * n as u64;
            #[allow(clippy::cast_precision_loss)]
            let per = |d: Duration| d.as_nanos() as f64 / sends as f64;
            println!(
                "N={n:>3}: buffered {:>6.1} ns/send | eager-Vec {:>6.1} ns/send",
                per(buffered),
                per(eager)
            );
        }
    }
}
