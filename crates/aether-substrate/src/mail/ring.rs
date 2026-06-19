//! `MailRing` — a single-producer, multi-consumer reclaiming byte ring
//! for blob-of-mail dispatch (ADR-0087, Phase 2 / iamacoffeepot/aether#1105).
//!
//! The ring is the substrate behind the blob axiom: a handler's outbound
//! mail is written into one contiguous region (the blob), and recipients
//! receive a `MailRef::InRing` ref into it instead of an owned `Vec<u8>`
//! copy. Two producer APIs build a blob:
//!
//! - [`MailRing::push_blob`] — atomic, whole-blob: all mails at once
//!   (used by tests / the microbench, and available as a convenience).
//! - [`MailRing::open_blob`] / [`MailRing::append`] / [`MailRing::seal`]
//!   — the incremental in-place FSM (2c, iamacoffeepot/aether#1110) the
//!   native dispatch path uses: each send is written straight into the
//!   ring as it happens, with no staging buffer.
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
//! - **One producer** (the owning actor's thread). It alone calls the
//!   build APIs ([`MailRing::push_blob`], or [`MailRing::open_blob`] /
//!   [`MailRing::append`] / [`MailRing::seal`]) and [`MailRing::reclaim`],
//!   and is the sole writer of the `write` / `front` cursors, the `build`
//!   FSM state, and the buffer bytes. Reclaim runs only at `open_blob`
//!   (never while a blob is open), so it never reads a half-built header.
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
/// to mint a `MailRef::InRing` and route it. `header_off` locates the
/// blob's `BlobHeader` for [`MailRing::release`]; `payload_off` /
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

/// Producer-only state for the blob being built in place via the
/// [`open_blob`](MailRing::open_blob) / [`append`](MailRing::append) /
/// [`seal`](MailRing::seal) FSM (ADR-0087 / 2c, iamacoffeepot/aether#1110).
/// Between `seal` and the next `open_blob` the state is `Closed`. The
/// blob's header is written by `append` only once the first mail lands
/// (so an open-but-empty blob writes nothing and `seal` is a clean
/// no-op), then finalized by `seal`.
#[derive(Clone, Copy)]
enum BuildState {
    /// No blob open. The resting state.
    Closed,
    /// A blob is open but no mail has been appended yet — no header
    /// placed.
    Empty,
    /// A blob with `n_mails` (>= 1) appended; its header sits at
    /// `header_off`, the next free byte is `cursor`. The header is
    /// provisional (`lock` / `total_len` unwritten) until `seal`.
    Building {
        header_off: u32,
        cursor: u32,
        n_mails: u32,
    },
}

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
    /// Producer-only FSM state for the in-place blob build (2c). Touched
    /// only by `open_blob` / `append` / `seal`, all on the producer
    /// thread — never by consumers — so it rides the same single-writer
    /// discipline as `write` / `front` / `live`.
    build: UnsafeCell<BuildState>,
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
    /// Allocate a ring with `cap` bytes (rounded up to `ALIGN`). `cap`
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
            build: UnsafeCell::new(BuildState::Closed),
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
    /// Returns [`RingFull`] if the blob does not fit — either transiently
    /// (the ring is full of un-reclaimed blobs) or structurally (the blob
    /// is larger than the whole ring). Both cases route to the caller's
    /// copy-out valve, which materializes the mails as owned buffers
    /// without blocking. A producer (a handler's fan-out) can emit an
    /// arbitrarily large blob, so an oversized one must degrade rather
    /// than panic the substrate (2b, iamacoffeepot/aether#1105).
    ///
    /// # Panics
    /// Panics if `mails` is empty (a blob always carries at least one
    /// mail).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "offsets are bounded by capacity, asserted <= u32::MAX in with_capacity"
    )]
    pub fn push_blob(&self, mails: &[OutMail<'_>]) -> Result<Vec<MailLoc>, RingFull> {
        assert!(!mails.is_empty(), "push_blob requires at least one mail");
        let size = Self::blob_size(mails);
        // A blob larger than the whole ring can never fit; hand it to the
        // copy-out valve instead of panicking.
        if size > self.cap {
            return Err(RingFull);
        }

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

        // Absorb a sub-header tail: `write` must never stop strictly
        // within `HEADER_LEN` bytes of `cap`, because a later wrap would
        // have to lay a filler header into a tail too small to hold one
        // (an out-of-bounds write). Cursor advances are multiples of
        // `ALIGN`, so the only reachable sub-header remainder is 8; pad
        // this blob's `total_len` to swallow it and wrap `write` to 0.
        // Invisible to consumers: `total_len` is read only as the reclaim
        // stride, while entry iteration is driven by `n_mails`.
        let end = start + size;
        let remainder = self.cap - end;
        let occupied = if remainder > 0 && remainder < HEADER_LEN {
            // SAFETY: `start` is the just-written blob's header in
            // producer-owned space; only the producer reads `total_len`.
            unsafe {
                let hdr = self.header_ptr(start);
                (&raw mut (*hdr).total_len).write((size + remainder) as u32);
            }
            size + remainder
        } else {
            size
        };
        let new_write = if start + occupied == self.cap {
            0
        } else {
            start + occupied
        };
        self.write.store(new_write as u32, Ordering::Relaxed);
        // SAFETY: producer-only mutation of the live counter.
        unsafe {
            *self.live.get() += occupied;
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
        // The sub-header-tail absorb in `push_blob` / `finalize_header`
        // guarantees `write` never stops strictly within `HEADER_LEN`
        // bytes of `cap`, so every filler region has room for its header.
        debug_assert!(
            len >= HEADER_LEN,
            "filler tail of {len} B cannot hold a {HEADER_LEN}-B header"
        );
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
                let loc = self.write_entry_at(cur, start, m.recipient, m.kind, m.payload);
                cur = loc.payload_off as usize + align_up(m.payload.len());
                locs.push(loc);
            }
            locs
        }
    }

    /// Write one mail's [`MailEntry`] + payload at `off` and return its
    /// [`MailLoc`] (carrying `header_off` for the consumer's later
    /// [`release`](Self::release)). Does **not** write the blob header —
    /// that is the atomic [`Self::write_blob_at`] (whole-blob) or
    /// [`Self::seal`] (incremental FSM, written once the blob is final).
    ///
    /// # Safety
    /// `off` must be producer-owned free space [`ALIGN`]-aligned with room
    /// for `ENTRY_LEN + payload.len()` bytes, and `header_off` must be the
    /// offset of the blob this entry belongs to.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "offsets and payload lengths are bounded by capacity, asserted <= u32::MAX in with_capacity"
    )]
    unsafe fn write_entry_at(
        &self,
        off: usize,
        header_off: usize,
        recipient: u64,
        kind: u64,
        payload: &[u8],
    ) -> MailLoc {
        // SAFETY: caller guarantees `off` is aligned, in-bounds free space
        // with room for the entry + payload.
        unsafe {
            let entry = MailEntry {
                len: payload.len() as u32,
                _pad: 0,
                recipient,
                kind,
            };
            self.buf
                .add(off)
                .copy_from_nonoverlapping((&raw const entry).cast::<u8>(), ENTRY_LEN);
            let payload_off = off + ENTRY_LEN;
            if !payload.is_empty() {
                self.buf
                    .add(payload_off)
                    .copy_from_nonoverlapping(payload.as_ptr(), payload.len());
            }
            MailLoc {
                recipient,
                kind,
                header_off: header_off as u32,
                payload_off: payload_off as u32,
                len: payload.len() as u32,
            }
        }
    }

    /// Begin building a new blob in place (ADR-0087 / 2c). The producer
    /// then [`append`](Self::append)s each mail and [`seal`](Self::seal)s
    /// at the end — payloads land in the ring once, with no staging
    /// buffer. **Producer-only.**
    ///
    /// Reclaims first: this is the *only* safe point to reclaim, because
    /// reclaim walks blob headers and an open blob's header is not yet
    /// finalized. No other method reclaims, so reclaim never races a
    /// half-built header.
    ///
    /// # Panics
    /// Panics if a blob is already open (`open_blob` without an
    /// intervening `seal`).
    pub fn open_blob(&self) {
        // SAFETY: producer-only state.
        let state = unsafe { &mut *self.build.get() };
        assert!(
            matches!(state, BuildState::Closed),
            "open_blob called while a blob is already open"
        );
        self.reclaim();
        *state = BuildState::Empty;
    }

    /// Append one mail to the open blob, writing its entry + payload into
    /// the ring in place, and return its [`MailLoc`]. **Producer-only.**
    ///
    /// Returns [`RingFull`] if the mail does not fit; the caller copies it
    /// out to an owned buffer (the never-block valve) and may keep
    /// appending — the open blob is left intact, so a later append (after
    /// a consumer frees space) can still extend it. When the mail would
    /// cross the buffer's end, the current blob is sealed early, a wrap
    /// filler is laid, and a fresh blob is opened at offset 0 (one
    /// handler's fan-out then spans two physical blobs — fine, each mail
    /// still gets its own `InRing` ref).
    ///
    /// # Panics
    /// Panics if no blob is open (`append` without [`Self::open_blob`]).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "cursor/offsets are bounded by capacity, asserted <= u32::MAX in with_capacity"
    )]
    pub fn append(&self, recipient: u64, kind: u64, payload: &[u8]) -> Result<MailLoc, RingFull> {
        let entry_size = ENTRY_LEN + align_up(payload.len());
        // SAFETY: producer-only state.
        let state = unsafe { &mut *self.build.get() };
        match *state {
            BuildState::Closed => panic!("append called without an open blob"),
            BuildState::Empty => {
                let start = self.place_blob_start(HEADER_LEN + entry_size)?;
                // SAFETY: `place_blob_start` returned a producer-owned,
                // ALIGN-aligned region with room for the header + this
                // entry; the entry goes right after the (not-yet-written)
                // header slot.
                let loc = unsafe {
                    self.write_entry_at(start + HEADER_LEN, start, recipient, kind, payload)
                };
                *state = BuildState::Building {
                    header_off: start as u32,
                    cursor: (start + HEADER_LEN + entry_size) as u32,
                    n_mails: 1,
                };
                Ok(loc)
            }
            BuildState::Building {
                header_off,
                cursor,
                n_mails,
            } => {
                let cur = cursor as usize;
                if cur + entry_size > self.cap {
                    // Crosses the buffer end: seal the current blob, then
                    // reopen at 0 (the recursive call lays the wrap filler
                    // via `place_blob_start` and writes the entry there).
                    self.finalize_header(header_off, cursor, n_mails);
                    *state = BuildState::Empty;
                    return self.append(recipient, kind, payload);
                }
                let open_bytes = cur - header_off as usize;
                if open_bytes + entry_size > self.free_bytes() {
                    // Ring full: leave the open blob as-is, caller spills
                    // this mail to an owned buffer.
                    return Err(RingFull);
                }
                // SAFETY: `cur` is the ALIGN-aligned tail of the open blob
                // in producer-owned free space, and the two checks above
                // guarantee `[cur, cur + entry_size)` is in-bounds and does
                // not overrun the live region.
                let loc = unsafe {
                    self.write_entry_at(cur, header_off as usize, recipient, kind, payload)
                };
                *state = BuildState::Building {
                    header_off,
                    cursor: (cur + entry_size) as u32,
                    n_mails: n_mails + 1,
                };
                Ok(loc)
            }
        }
    }

    /// Seal the open blob: finalize its header (`total_len`, publish
    /// `lock = n_mails`), advance `write`, and add it to the live set.
    /// **Producer-only.** A no-op for an open-but-empty blob (every mail
    /// spilled to the copy-out valve, or no sends happened).
    ///
    /// # Panics
    /// Panics if no blob is open (`seal` without [`Self::open_blob`]).
    pub fn seal(&self) {
        // SAFETY: producer-only state.
        let state = unsafe { &mut *self.build.get() };
        match *state {
            BuildState::Closed => panic!("seal called without an open blob"),
            BuildState::Empty => {}
            BuildState::Building {
                header_off,
                cursor,
                n_mails,
            } => self.finalize_header(header_off, cursor, n_mails),
        }
        *state = BuildState::Closed;
    }

    /// Pick the start offset for a blob needing `needed` contiguous bytes
    /// (header + first entry), wrapping with a tail filler if the tail is
    /// too small, and leave `write` pointing at the chosen start. Mirrors
    /// the placement [`push_blob`](Self::push_blob) does atomically.
    /// **Producer-only.** Returns [`RingFull`] if it doesn't fit even
    /// after a wrap (nothing is mutated in that case).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "offsets are bounded by capacity, asserted <= u32::MAX in with_capacity"
    )]
    fn place_blob_start(&self, needed: usize) -> Result<usize, RingFull> {
        let write = self.write.load(Ordering::Relaxed) as usize;
        let tail_room = self.cap - write;
        if needed <= tail_room {
            if needed > self.free_bytes() {
                return Err(RingFull);
            }
            Ok(write)
        } else {
            // Wrap: a filler over `[write, cap)` plus the blob at 0. Both
            // must fit in current free space (same check as `push_blob`).
            if tail_room + needed > self.free_bytes() {
                return Err(RingFull);
            }
            if tail_room > 0 {
                self.write_filler(write, tail_room);
                // SAFETY: producer-only counter; the filler bytes are now
                // live until reclaimed.
                unsafe {
                    *self.live.get() += tail_room;
                }
            }
            self.write.store(0, Ordering::Relaxed);
            Ok(0)
        }
    }

    /// Finalize a built blob's header at `header_off` (publish
    /// `lock = n_mails`, write `total_len`), advance `write` past it, and
    /// add its bytes to the live set. Shared by [`Self::seal`] and the
    /// seal-early wrap in [`Self::append`]. **Producer-only.**
    #[allow(
        clippy::cast_possible_truncation,
        reason = "offsets are bounded by capacity, asserted <= u32::MAX in with_capacity"
    )]
    fn finalize_header(&self, header_off: u32, cursor: u32, n_mails: u32) {
        let header_off = header_off as usize;
        let cursor = cursor as usize;
        let mut total = cursor - header_off;
        // Absorb a sub-header tail (same invariant as `push_blob`):
        // `write` must never stop strictly within `HEADER_LEN` bytes of
        // `cap`, or a later wrap filler would write its header out of
        // bounds. Pad `total_len` to swallow the remainder and wrap
        // `write` to 0; reclaim strides `front + total == cap → 0` with
        // no consumer-side change.
        let remainder = self.cap - cursor;
        if remainder > 0 && remainder < HEADER_LEN {
            total += remainder;
        }
        // SAFETY: `[header_off, cursor)` is the producer-owned region just
        // written by `append` (and a sub-header remainder up to `cap` is
        // producer-owned free space); we write the header fields once.
        // The lock's initialized value is published to consumers through
        // the inbox channel handoff of the minted `MailRef` (see module
        // docs), so a plain write suffices here.
        unsafe {
            let hdr = self.header_ptr(header_off);
            (&raw mut (*hdr).lock).write(AtomicU32::new(n_mails));
            (&raw mut (*hdr).n_mails).write(n_mails);
            (&raw mut (*hdr).total_len).write(total as u32);
            (&raw mut (*hdr).is_filler).write(0);
            *self.live.get() += total;
        }
        let end = header_off + total;
        let new_write = if end == self.cap { 0 } else { end };
        self.write.store(new_write as u32, Ordering::Relaxed);
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
impl MailRing {
    /// Batch-decrement a blob's lock by `n`. **Producer or consumer-safe**
    /// — it is the same atomic as [`Self::release`]. Test-only: the
    /// production inline-drain path releases one count at a time via
    /// [`Self::release`]; this batched form has no production callers and
    /// only exists to exercise the reclaim arithmetic in
    /// [`tests::batch_release_reclaims`].
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
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::undocumented_unsafe_blocks,
    clippy::cast_possible_truncation,
    clippy::needless_collect,
    reason = "test code: unwraps assert via panic; unsafe blocks exercise the ring API whose safety contracts are documented on the methods and upheld by construction here; casts are capacity-bounded; the consumer-handle collect forces every thread to spawn before any join"
)]
#[allow(clippy::disallowed_methods)] // test scaffolding — threads here hold no settlement contract
mod tests {
    use std::collections::BTreeSet;

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
    fn oversized_blob_returns_ring_full_not_panic() {
        // A single blob larger than the whole ring degrades to the
        // copy-out valve rather than panicking (2b: handler fan-out is
        // unbounded; the substrate must not crash on a big blob).
        let ring = MailRing::with_capacity(128);
        let r = ring.push_blob(&[out(1, 1, &[0; 256])]);
        assert_eq!(r, Err(RingFull));
        // The ring is untouched — a later in-bounds blob still fits.
        assert_eq!(ring.live_bytes(), 0);
        assert!(ring.push_blob(&[out(2, 2, &[7; 16])]).is_ok());
    }

    #[test]
    fn open_append_seal_round_trips() {
        let ring = MailRing::with_capacity(4096);
        ring.open_blob();
        let l0 = ring.append(1, 10, &[1, 2, 3]).expect("first appends");
        let l1 = ring.append(2, 20, &[4, 5, 6, 7]).expect("second appends");
        ring.seal();
        // Both mails landed in one blob (same header).
        assert_eq!(l0.header_off, l1.header_off);
        assert_eq!(l0.recipient, 1);
        assert_eq!(l1.kind, 20);
        // SAFETY: locks held (not released) until below; region is live.
        unsafe {
            assert_eq!(ring.payload(l0.payload_off, l0.len), &[1, 2, 3]);
            assert_eq!(ring.payload(l1.payload_off, l1.len), &[4, 5, 6, 7]);
        }
        let live = ring.live_bytes();
        assert!(live > 0);
        // Lock is n_mails (2): one release isn't enough.
        // SAFETY: each ref releases its one held count.
        unsafe { ring.release(l0.header_off) };
        assert_eq!(ring.reclaim(), 0);
        // SAFETY: second (last) release.
        unsafe { ring.release(l1.header_off) };
        assert_eq!(ring.reclaim(), live);
        assert_eq!(ring.live_bytes(), 0);
    }

    #[test]
    fn fsm_empty_seal_is_noop() {
        let ring = MailRing::with_capacity(256);
        ring.open_blob();
        // No mails appended — seal writes nothing.
        ring.seal();
        assert_eq!(ring.live_bytes(), 0);
        // The ring is still fully usable afterward.
        ring.open_blob();
        let l = ring.append(1, 1, &[9, 9]).expect("appends");
        ring.seal();
        // SAFETY: lock held.
        unsafe {
            assert_eq!(ring.payload(l.payload_off, l.len), &[9, 9]);
            ring.release(l.header_off);
        }
        assert!(ring.reclaim() > 0);
    }

    #[test]
    fn fsm_append_returns_ring_full_when_no_space() {
        let ring = MailRing::with_capacity(128);
        ring.open_blob();
        // One 64-byte mail fits (16 header + 24 entry + 64 payload = 104).
        let _l = ring.append(1, 1, &[0; 64]).expect("first fits");
        // A second won't fit and can't wrap into the (full) ring.
        let r = ring.append(2, 2, &[0; 64]);
        assert_eq!(r, Err(RingFull));
        ring.seal();
    }

    #[test]
    fn fan_out_spanning_tail_uses_two_blobs() {
        // Advance `write` toward the tail while keeping the ring free:
        // push → release → reclaim marches `write`/`front` forward together.
        let ring = MailRing::with_capacity(256);
        for _ in 0..3 {
            let l = ring.push_blob(&[out(9, 9, &[0; 8])]).unwrap(); // 48 B each
            // SAFETY: single held count, released immediately.
            unsafe { ring.release(l[0].header_off) };
            ring.reclaim();
        }
        // `write` now sits at 144 with the ring otherwise free. Build a wide
        // fan-out that must cross the 256-byte tail mid-build.
        ring.open_blob();
        let mut locs = Vec::new();
        for k in 0..7u64 {
            locs.push(
                ring.append(k, k, &[0xAA; 8])
                    .expect("appends fit the free ring"),
            );
        }
        ring.seal();
        // The fan-out crossed the tail, so it spans >= 2 physical blobs.
        let distinct: BTreeSet<u32> = locs.iter().map(|l| l.header_off).collect();
        assert!(
            distinct.len() >= 2,
            "fan-out should span >=2 blobs across the wrap; header_offs: {distinct:?}"
        );
        // Every payload still reads back intact across the split.
        for l in &locs {
            // SAFETY: locks held until released below.
            let bytes = unsafe { ring.payload(l.payload_off, l.len) };
            assert!(
                bytes.iter().all(|&b| b == 0xAA),
                "payload corrupted across wrap"
            );
        }
        for l in &locs {
            // SAFETY: one held count per loc.
            unsafe { ring.release(l.header_off) };
        }
        while ring.reclaim() > 0 {}
        assert_eq!(ring.live_bytes(), 0);
    }

    #[test]
    fn fsm_build_many_cycles_round_trips() {
        // Single-threaded soak of open/append/seal over a small ring:
        // wraps (placement + seal-early) and reclaim recur as `write`
        // circles. A reused-region bug would show as a tag mismatch.
        let ring = MailRing::with_capacity(512);
        for i in 0..2000u32 {
            let tag = (i & 0xff) as u8;
            let n = (i % 4 + 1) as usize;
            ring.open_blob();
            let mut locs = Vec::new();
            for k in 0..n {
                let payload = vec![tag; 8 + k * 8];
                if let Ok(loc) = ring.append(k as u64, u64::from(tag), &payload) {
                    locs.push((loc, payload.len()));
                }
                // A RingFull here would (in production) spill to Owned; the
                // free-every-cycle pattern keeps the ring from filling, so
                // we simply don't record a spilled mail.
            }
            ring.seal();
            for (loc, len) in &locs {
                assert_eq!(loc.len as usize, *len);
                // SAFETY: lock held until the release loop below.
                let bytes = unsafe { ring.payload(loc.payload_off, loc.len) };
                assert!(
                    bytes.iter().all(|&b| b == tag),
                    "cycle {i}: tag {tag} corrupted"
                );
            }
            for (loc, _) in &locs {
                // SAFETY: one held count per loc.
                unsafe { ring.release(loc.header_off) };
            }
            ring.reclaim();
        }
        while ring.reclaim() > 0 {}
        assert_eq!(ring.live_bytes(), 0);
    }

    #[test]
    fn atomic_push_absorbs_sub_header_tail() {
        // Regression for iamacoffeepot/aether#1530: drive `write` to
        // exactly `cap - 8` — the one reachable sub-header tail — and
        // verify the last blob absorbed the remainder instead of leaving
        // a tail too small for a wrap filler's header (an OOB write).
        // Blob sizes: empty payload = 40 B, 8-byte payload = 48 B;
        // 40*5 + 48*81 = 4088 = 4096 - 8.
        let ring = MailRing::with_capacity(4096);
        let mut locs = Vec::new();
        for i in 0..5u64 {
            locs.extend(ring.push_blob(&[out(i, i, &[])]).unwrap());
        }
        for i in 0..81u64 {
            locs.extend(ring.push_blob(&[out(i, i, &[0xCD; 8])]).unwrap());
        }
        // The absorb padded the last blob's total_len by 8 and wrapped
        // `write` to 0, so the whole ring is live (4088 written + 8
        // absorbed). Without the absorb this reads 4088.
        assert_eq!(ring.live_bytes(), 4096);
        // The absorbed blob's payload is intact (padding is invisible).
        let last = *locs.last().unwrap();
        // SAFETY: lock held until the release loop below.
        unsafe { assert_eq!(ring.payload(last.payload_off, last.len), &[0xCD; 8]) };
        for l in &locs {
            // SAFETY: one held count per loc.
            unsafe { ring.release(l.header_off) };
        }
        // Reclaim strides the padded blob `front + total == cap → 0` —
        // an unpadded 48-byte stride would park `front` at 4088 and read
        // garbage header bytes there.
        assert_eq!(ring.reclaim(), 4096);
        assert_eq!(ring.live_bytes(), 0);
        // The ring stays fully usable: the next blob lands at offset 0.
        let l = ring.push_blob(&[out(7, 7, &[0xEE; 8])]).unwrap();
        assert_eq!(l[0].header_off, 0);
        // SAFETY: lock held, released after the read.
        unsafe {
            assert_eq!(ring.payload(l[0].payload_off, l[0].len), &[0xEE; 8]);
            ring.release(l[0].header_off);
        }
        assert!(ring.reclaim() > 0);
        assert_eq!(ring.live_bytes(), 0);
    }

    #[test]
    fn incremental_seal_absorbs_sub_header_tail() {
        // Regression for iamacoffeepot/aether#1530, append/seal path:
        // a sealed blob ending at exactly `cap - 8` absorbs the
        // sub-header remainder in `finalize_header`.
        let ring = MailRing::with_capacity(4096);
        // March `write`/`front` to 4000 with 100 push/release/reclaim
        // cycles of a 40-byte blob.
        for _ in 0..100 {
            let l = ring.push_blob(&[out(1, 1, &[])]).unwrap();
            // SAFETY: single held count, released immediately.
            unsafe { ring.release(l[0].header_off) };
            ring.reclaim();
        }
        // Build a blob of 16 + (24 + 48) = 88 bytes at 4000: it ends at
        // 4088 = cap - 8, and seal absorbs the 8-byte remainder.
        ring.open_blob();
        let l = ring.append(2, 2, &[0xAB; 48]).unwrap();
        ring.seal();
        // 88 written + 8 absorbed; without the absorb this reads 88.
        assert_eq!(ring.live_bytes(), 96);
        // SAFETY: lock held until the release below.
        unsafe { assert_eq!(ring.payload(l.payload_off, l.len), &[0xAB; 48]) };
        // SAFETY: the blob's single held count.
        unsafe { ring.release(l.header_off) };
        // Reclaim strides `4000 + 96 == cap → 0`.
        assert_eq!(ring.reclaim(), 96);
        assert_eq!(ring.live_bytes(), 0);
        // The next incremental blob lands at offset 0 and round-trips.
        ring.open_blob();
        let l = ring.append(3, 3, &[0x5A; 8]).unwrap();
        ring.seal();
        assert_eq!(l.header_off, 0);
        // SAFETY: lock held, released after the read.
        unsafe {
            assert_eq!(ring.payload(l.payload_off, l.len), &[0x5A; 8]);
            ring.release(l.header_off);
        }
        assert!(ring.reclaim() > 0);
        assert_eq!(ring.live_bytes(), 0);
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

    /// Property-based state machine over the ring (issue 1561). Random
    /// sequences of `push` / `open-append-seal` / `release` / `reclaim`
    /// run single-threaded against a small ring — the test plays both
    /// producer and consumer, sound because the single-writer discipline's
    /// happens-before edges are trivial on one thread. A shadow model
    /// tracks live blobs (offset, payload bytes, outstanding lock) and
    /// asserts the ring invariants after every op:
    ///
    /// - `live_bytes <= capacity` (no cursor crosses `cap`);
    /// - every held blob's payload reads back what was written (no two
    ///   live blobs overlap, no region reused while a lock is held);
    /// - releases never underflow a lock;
    /// - reclaim's freed-byte count matches the live-counter drop, and a
    ///   zero-lock blob at the head always reclaims.
    use std::collections::VecDeque;

    use proptest::prelude::*;

    /// Small ring so wraps, fillers, and `RingFull` all fire within a
    /// short op sequence.
    const RING_CAP: usize = 256;
    const MAX_PAYLOAD: usize = 40;
    const MAX_MAILS: usize = 4;

    /// One driver op. `Release(idx)` picks a held blob by index modulo
    /// the live count at apply time (the upfront `Vec<Op>` can't name a
    /// runtime `header_off`); `Reclaim` takes no parameter.
    #[derive(Debug, Clone)]
    enum Op {
        PushBlob(Vec<Vec<u8>>),
        OpenAppendSeal(Vec<Vec<u8>>),
        Release(usize),
        Reclaim,
    }

    fn arb_payload() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(any::<u8>(), 0..=MAX_PAYLOAD)
    }

    fn arb_op() -> impl Strategy<Value = Op> {
        prop_oneof![
            prop::collection::vec(arb_payload(), 1..=MAX_MAILS).prop_map(Op::PushBlob),
            prop::collection::vec(arb_payload(), 0..=MAX_MAILS).prop_map(Op::OpenAppendSeal),
            any::<usize>().prop_map(Op::Release),
            Just(Op::Reclaim),
        ]
    }

    /// One held mail ref: where to read and the bytes that must be
    /// there.
    struct ModelRef {
        payload_off: u32,
        len: u32,
        bytes: Vec<u8>,
    }

    /// One live blob in the shadow model. `outstanding` mirrors the
    /// blob's atomic lock (init = mail count, decremented per release).
    struct ModelBlob {
        header_off: u32,
        outstanding: u32,
        refs: Vec<ModelRef>,
    }

    /// Pop the front-contiguous run of fully-released (`outstanding == 0`)
    /// blobs, mirroring the ring's FIFO reclaim. Returns how many were
    /// popped.
    fn model_reclaim_front(fifo: &mut VecDeque<ModelBlob>) -> usize {
        let mut popped = 0;
        while let Some(front) = fifo.front() {
            if front.outstanding == 0 {
                fifo.pop_front();
                popped += 1;
            } else {
                break;
            }
        }
        popped
    }

    /// Assert the per-op invariants against the live model.
    fn check_invariants(ring: &MailRing, fifo: &VecDeque<ModelBlob>) {
        assert!(
            ring.live_bytes() <= ring.capacity(),
            "live_bytes {} exceeds capacity {}",
            ring.live_bytes(),
            ring.capacity()
        );
        for blob in fifo {
            if blob.outstanding == 0 {
                // Released-but-not-yet-reclaimed: the region is
                // reclaimable, so the lock-held read contract no longer
                // applies — skip it.
                continue;
            }
            for r in &blob.refs {
                // SAFETY: outstanding > 0 means this blob's lock is held,
                // so the producer cannot have reclaimed or overwritten the
                // region — the read is sound.
                let bytes = unsafe { ring.payload(r.payload_off, r.len) };
                assert_eq!(
                    bytes,
                    &r.bytes[..],
                    "payload read-back mismatch at offset {} (region reused under a live lock)",
                    r.payload_off
                );
            }
        }
    }

    /// Group consecutively-appended mails into model blobs by their
    /// `header_off` — a tail-spanning fan-out seals early and reopens at
    /// 0, so one `open/append/seal` cycle can produce two physical
    /// blobs, each with its own lock.
    fn record_appended(fifo: &mut VecDeque<ModelBlob>, appended: &[(MailLoc, Vec<u8>)]) {
        let mut i = 0;
        while i < appended.len() {
            let header_off = appended[i].0.header_off;
            let mut refs = Vec::new();
            while i < appended.len() && appended[i].0.header_off == header_off {
                let (loc, bytes) = &appended[i];
                refs.push(ModelRef {
                    payload_off: loc.payload_off,
                    len: loc.len,
                    bytes: bytes.clone(),
                });
                i += 1;
            }
            let outstanding = refs.len() as u32;
            fifo.push_back(ModelBlob {
                header_off,
                outstanding,
                refs,
            });
        }
    }

    /// Replay an op sequence against a fresh ring + shadow model.
    fn apply(ops: Vec<Op>) {
        let ring = MailRing::with_capacity(RING_CAP);
        let mut fifo: VecDeque<ModelBlob> = VecDeque::new();

        for op in ops {
            match op {
                Op::PushBlob(payloads) => {
                    let mails: Vec<OutMail<'_>> = payloads
                        .iter()
                        .enumerate()
                        .map(|(k, p)| OutMail {
                            recipient: k as u64,
                            kind: k as u64,
                            payload: p,
                        })
                        .collect();
                    if let Ok(locs) = ring.push_blob(&mails) {
                        let header_off = locs[0].header_off;
                        let refs = locs
                            .iter()
                            .zip(&payloads)
                            .map(|(l, p)| ModelRef {
                                payload_off: l.payload_off,
                                len: l.len,
                                bytes: p.clone(),
                            })
                            .collect();
                        fifo.push_back(ModelBlob {
                            header_off,
                            outstanding: locs.len() as u32,
                            refs,
                        });
                    }
                    // RingFull leaves the ring untouched — no model change.
                }
                Op::OpenAppendSeal(payloads) => {
                    // open_blob reclaims internally; mirror that in the
                    // model so the FIFO front stays in lockstep.
                    ring.open_blob();
                    model_reclaim_front(&mut fifo);
                    let mut appended = Vec::new();
                    for (k, p) in payloads.iter().enumerate() {
                        if let Ok(loc) = ring.append(k as u64, k as u64, p) {
                            appended.push((loc, p.clone()));
                        }
                        // RingFull spills this mail; the open blob stays
                        // intact and later appends may still land.
                    }
                    ring.seal();
                    record_appended(&mut fifo, &appended);
                }
                Op::Release(idx) => {
                    let releasable: Vec<usize> = fifo
                        .iter()
                        .enumerate()
                        .filter(|(_, b)| b.outstanding > 0)
                        .map(|(i, _)| i)
                        .collect();
                    if !releasable.is_empty() {
                        let chosen = releasable[idx % releasable.len()];
                        assert!(
                            fifo[chosen].outstanding > 0,
                            "release must never underflow a lock"
                        );
                        // SAFETY: the model holds exactly one count of this
                        // blob's lock per outstanding ref; releasing it once
                        // matches that held count.
                        unsafe { ring.release(fifo[chosen].header_off) };
                        fifo[chosen].outstanding -= 1;
                    }
                }
                Op::Reclaim => {
                    let live_before = ring.live_bytes();
                    let reclaimed = ring.reclaim();
                    assert_eq!(
                        live_before - reclaimed,
                        ring.live_bytes(),
                        "reclaim's freed-byte count must match the live-counter drop"
                    );
                    let popped = model_reclaim_front(&mut fifo);
                    if popped > 0 {
                        // A zero-lock blob sat at the head, so reclaim must
                        // have advanced past it. (The converse isn't
                        // asserted: a tail filler can free bytes ahead of a
                        // still-locked blob.)
                        assert!(
                            reclaimed > 0,
                            "front had {popped} zero-lock blob(s) but reclaim freed nothing"
                        );
                    }
                }
            }
            check_invariants(&ring, &fifo);
        }

        // Drain: release every held ref, reclaim to a fixpoint, and the
        // ring must report zero live bytes.
        for blob in &mut fifo {
            while blob.outstanding > 0 {
                // SAFETY: one release per remaining held count.
                unsafe { ring.release(blob.header_off) };
                blob.outstanding -= 1;
            }
        }
        while ring.reclaim() > 0 {}
        assert_eq!(
            ring.live_bytes(),
            0,
            "every blob reclaims once fully released"
        );
    }

    proptest! {
        /// Default 256 cases of up-to-64-op sequences.
        #[test]
        fn ring_op_sequence_preserves_invariants(
            ops in prop::collection::vec(arb_op(), 0..=64)
        ) {
            apply(ops);
        }
    }
}
