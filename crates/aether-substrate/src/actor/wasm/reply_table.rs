// Per-mailbox-slot mapping from guest-visible reply handles
// (opaque `u32`) to the substrate-internal reply destination. Handles
// are allocated when a component receives mail that carries a reply
// target and resolved when the guest calls `reply_mail` to answer.
//
// ADR-0013 covered session-bound replies; ADR-0017 widened the table
// so component-originated mail also produces a handle — a component
// can now reply to a runtime-discovered peer without knowing its
// name at init, using the same `ctx.reply` API regardless of who
// called. ADR-0037 widened it again with a remote-engine variant.
//
// The table is a generation-tagged slab (#6412). A handle packs a slot
// index into its low 20 bits and that slot's generation into its high
// 12 bits, so it still fits the guest-visible `u32` of the `receive_p32`
// and `reply_mail_p32` ABI. Freeing a slot advances its generation and
// queues the index at the back of a first-in first-out free queue, so a
// late or duplicate reply that carries the old handle finds a generation
// mismatch instead of the slot's new requester. The queue starts holding
// every reserved index, so a component that frees each handle at once
// cycles through all of them and one slot's generation advances only
// once per full cycle.
//
// The table reserves 256 slots on its first allocation. When every slot
// is held it grows by one slot and never drops a held handle; each time
// the live count passes a new high-water mark (the reservation, then each
// doubling) `high_water` reports it once so the component can warn with
// the actor's name. At 2^20 held handles, the most the index bits
// address, `allocate` refuses and the component fails fast.
//
// An entry is freed when the guest answers it, when a single-class
// dispatch returns (the guest's `DISPATCH_HANDLED_RELEASE`, ADR-0112), or
// when a delivery is unhandled (`DISPATCH_UNKNOWN_KIND`). A handle a
// manual handler or a `#[fallback]` keeps lives until it is answered.
//
// The table lives on `ComponentCtx` rather than `Component` because
// the host fn touches it via `Caller::data_mut()`. The ctx dies with
// its instance, so the component trampoline moves the table out as an
// opaque `PendingReplies` when a guest leaves the slot and installs it
// on the next occupant — across replace, drop-then-refill and a
// replacement that fails to start — so a held handle still answers its
// own requester and the free queue carries on (#6409).

use std::collections::VecDeque;

use aether_data::SessionToken;

use crate::mail::{MailboxId, SourceAddr};

/// Sentinel passed to the guest's `receive` shim when the inbound
/// mail has no reply target (broadcast origin — ADR-0013 §1). A
/// `reply_mail` call with this handle fails with the "unknown
/// handle" status.
pub const NO_REPLY_HANDLE: u32 = u32::MAX;

/// What a reply handle resolves to on the substrate side. The guest
/// sees only the opaque `u32` — the `addr` variant lets `reply_mail`
/// pick the right outbound route, and `correlation_id` carries the
/// ADR-0042 correlation from the inbound mail so the reply's echo
/// happens automatically when `reply_mail` constructs the outbound
/// `Source`.
///
/// Invariant: `addr` is never `SourceAddr::None` — the table only
/// allocates entries for mail that had a meaningful sender addr.
/// The shared enum stays convenient (same shape as envelope-level
/// `Source.addr`), at the cost of a dead `None` variant here.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReplyEntry {
    pub addr: SourceAddr,
    pub correlation_id: u64,
}

impl ReplyEntry {
    /// Short constructor: `addr` + `correlation_id`.
    #[must_use]
    pub fn new(addr: SourceAddr, correlation_id: u64) -> Self {
        Self { addr, correlation_id }
    }

    /// Back-compat shim for call sites that used the pre-correlation
    /// `ReplyEntry::Session(token)` form. Builds an entry with no
    /// correlation.
    #[must_use]
    pub fn session(token: SessionToken) -> Self {
        Self::new(SourceAddr::Session(token), 0)
    }

    /// Back-compat shim for `ReplyEntry::Component(mailbox)`.
    #[must_use]
    pub fn component(mailbox: MailboxId) -> Self {
        Self::new(SourceAddr::Component(mailbox), 0)
    }
}

/// Low bits of a handle that hold the slot index. The rest of the `u32`
/// holds the slot's generation, so the handle keeps the guest ABI's width.
const INDEX_BITS: u32 = 20;

/// Mask selecting the slot index from a handle.
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;

/// The most slots the index bits address: the ceiling on held handles, past
/// which `allocate` refuses rather than aliasing a held slot.
const MAX_SLOTS: u32 = 1 << INDEX_BITS;

/// The number of generations one slot cycles through before a stale handle
/// for it could match again.
const GENERATION_LIMIT: u32 = 1 << (32 - INDEX_BITS);

/// Slots reserved on the first allocation. Well above the handles a
/// component holds at once — the dispatch in progress plus one per
/// outstanding deferred request — for about 12 KiB, and at 60 dispatches a
/// second it puts a slot's generation wrap about five hours apart.
const PREALLOCATED_REPLY_SLOTS: u32 = 256;

/// Pack a slot index and its generation into a guest-visible handle.
const fn pack(index: u32, generation: u32) -> u32 {
    (generation << INDEX_BITS) | index
}

/// Split a guest-supplied handle into its slot index and generation.
const fn unpack(handle: u32) -> (u32, u32) {
    (handle & INDEX_MASK, handle >> INDEX_BITS)
}

/// The generation a slot takes when it is freed: the next one modulo
/// [`GENERATION_LIMIT`], skipping the one that would pack to
/// [`NO_REPLY_HANDLE`] (only the last index has one).
const fn next_generation(index: u32, generation: u32) -> u32 {
    let next = (generation + 1) % GENERATION_LIMIT;
    if pack(index, next) == NO_REPLY_HANDLE {
        (next + 1) % GENERATION_LIMIT
    } else {
        next
    }
}

/// One slab slot: the generation its current or next handle carries, and
/// the entry while a handle is held.
#[derive(Debug, Default)]
struct Slot {
    generation: u32,
    entry: Option<ReplyEntry>,
}

/// Maintains the handle→entry slab for one mailbox slot.
#[derive(Debug)]
pub struct ReplyTable {
    slots: Vec<Slot>,
    free: VecDeque<u32>,
    live: usize,
    preallocated: u32,
    next_warning: usize,
}

impl Default for ReplyTable {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free: VecDeque::new(),
            live: 0,
            preallocated: PREALLOCATED_REPLY_SLOTS,
            next_warning: PREALLOCATED_REPLY_SLOTS as usize,
        }
    }
}

impl ReplyTable {
    /// An empty table. It reserves nothing until its first allocation, so the
    /// placeholder a moved-out table leaves behind costs nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A table that reserves `slots` instead of the production reservation.
    #[cfg(test)]
    fn with_preallocated(slots: u32) -> Self {
        Self { preallocated: slots, next_warning: slots as usize, ..Self::default() }
    }

    /// Allocate a fresh handle bound to `entry`: the oldest freed slot under
    /// its next generation, else a new slot. The returned handle is never
    /// `NO_REPLY_HANDLE`. Returns `None` only when all 2^20 addressable slots
    /// are held.
    pub fn allocate(&mut self, entry: ReplyEntry) -> Option<u32> {
        if self.slots.is_empty() {
            self.slots.resize_with(self.preallocated as usize, Slot::default);
            self.free.extend(0..self.preallocated);
        }
        let index = if let Some(index) = self.free.pop_front() {
            index
        } else {
            let index = u32::try_from(self.slots.len()).ok().filter(|index| *index < MAX_SLOTS)?;
            self.slots.push(Slot::default());
            index
        };
        let slot = &mut self.slots[index as usize];
        slot.entry = Some(entry);
        self.live += 1;
        Some(pack(index, slot.generation))
    }

    /// Look up the entry for a guest-supplied handle. Returns `None`
    /// for `NO_REPLY_HANDLE`, for handles that were never allocated, and
    /// for a stale handle whose slot has since been freed or reused.
    #[must_use]
    pub fn resolve(&self, handle: u32) -> Option<ReplyEntry> {
        self.held(handle).and_then(|slot| slot.entry)
    }

    /// Look up and remove the entry for a guest-supplied handle. A
    /// handle is one-shot: this is the production resolve path
    /// (`reply_mail`) and the release after a single-class or unhandled
    /// dispatch. It frees the slot under its next generation, so the
    /// table holds only unanswered handles, never lifetime traffic.
    /// Returns `None` for `NO_REPLY_HANDLE`, for handles that were never
    /// allocated, and for handles already taken.
    pub fn take(&mut self, handle: u32) -> Option<ReplyEntry> {
        if handle == NO_REPLY_HANDLE {
            return None;
        }
        let (index, generation) = unpack(handle);
        let slot = self.slots.get_mut(index as usize).filter(|slot| slot.generation == generation)?;
        let entry = slot.entry.take()?;
        slot.generation = next_generation(index, generation);
        self.free.push_back(index);
        self.live -= 1;
        Some(entry)
    }

    /// The live handle count, once each time it passes the next high-water
    /// mark — the reservation, then each doubling — else `None`. The caller
    /// warns on `Some`, so a component whose held handles keep growing is
    /// named once per doubling rather than on every allocation.
    pub(crate) fn high_water(&mut self) -> Option<usize> {
        if self.live <= self.next_warning {
            return None;
        }
        while self.next_warning < self.live {
            self.next_warning = (self.next_warning * 2).max(1);
        }
        Some(self.live)
    }

    /// The slot a handle names, if its generation is current.
    fn held(&self, handle: u32) -> Option<&Slot> {
        if handle == NO_REPLY_HANDLE {
            return None;
        }
        let (index, generation) = unpack(handle);
        self.slots.get(index as usize).filter(|slot| slot.generation == generation)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use aether_data::Uuid;

    use super::*;

    fn token(byte: u8) -> SessionToken {
        SessionToken(Uuid::from_bytes([byte; 16]))
    }

    #[test]
    fn allocate_session_and_component_handles_roundtrip() {
        let mut t = ReplyTable::new();
        let h_sess = t.allocate(ReplyEntry::session(token(1))).expect("room");
        let h_comp = t.allocate(ReplyEntry::component(MailboxId(42))).expect("room");
        assert_ne!(h_sess, h_comp);
        assert_eq!(t.resolve(h_sess), Some(ReplyEntry::session(token(1))));
        assert_eq!(t.resolve(h_comp), Some(ReplyEntry::component(MailboxId(42))));
    }

    #[test]
    fn resolve_sentinel_is_none() {
        let t = ReplyTable::new();
        assert!(t.resolve(NO_REPLY_HANDLE).is_none());
    }

    #[test]
    fn resolve_unknown_handle_is_none() {
        let mut t = ReplyTable::new();
        let _ = t.allocate(ReplyEntry::session(token(7)));
        assert!(t.resolve(9999).is_none());
    }

    #[test]
    fn next_generation_skips_the_sentinel_on_the_last_slot() {
        // Tripwire: the last index at the last generation packs to
        // `u32::MAX`, which the guest reads as "no reply target", so the
        // reply would be lost; the generation after the one before it
        // must skip that value.
        let generation = next_generation(MAX_SLOTS - 1, GENERATION_LIMIT - 2);
        assert_ne!(pack(MAX_SLOTS - 1, generation), NO_REPLY_HANDLE);
    }

    #[test]
    fn a_late_reply_to_a_reused_slot_is_refused() {
        let mut t = ReplyTable::with_preallocated(1);
        let a = ReplyEntry::session(token(1));
        let b = ReplyEntry::session(token(2));
        let first = t.allocate(a).expect("room");
        assert_eq!(t.take(first), Some(a));
        let second = t.allocate(b).expect("room");

        assert_ne!(first, second);
        assert_eq!(unpack(first).0, unpack(second).0);
        assert_eq!(t.take(first), None);
        assert_eq!(t.resolve(second), Some(b));
    }

    #[test]
    fn a_full_table_grows_and_warns_once_per_doubling() {
        let mut t = ReplyTable::with_preallocated(2);
        let mut handles = Vec::new();
        let mut marks = Vec::new();
        for byte in 0..5 {
            handles.push(t.allocate(ReplyEntry::session(token(byte))).expect("room"));
            marks.push(t.high_water());
        }

        assert_eq!(marks, [None, None, Some(3), None, Some(5)]);
        for (byte, handle) in (0..5).zip(&handles) {
            assert_eq!(t.resolve(*handle), Some(ReplyEntry::session(token(byte))));
        }

        for handle in handles {
            assert!(t.take(handle).is_some());
        }
        for byte in 0..5 {
            t.allocate(ReplyEntry::session(token(byte))).expect("room");
            assert_eq!(t.high_water(), None);
        }
    }

    #[test]
    fn allocation_at_the_index_ceiling_is_refused() {
        let mut t = ReplyTable::new();
        let entry = ReplyEntry::session(token(4));
        let mut seen = HashSet::new();
        for _ in 0..MAX_SLOTS {
            assert!(seen.insert(t.allocate(entry).expect("below the ceiling")));
        }

        assert_eq!(t.allocate(entry), None);
    }

    #[test]
    fn allocate_preserves_correlation_id() {
        let mut t = ReplyTable::new();
        let entry = ReplyEntry::new(SourceAddr::Session(token(5)), 0xCAFE_BABE);
        let h = t.allocate(entry).expect("room");
        let got = t.resolve(h).expect("resolves");
        assert_eq!(got.correlation_id, 0xCAFE_BABE);
    }

    #[test]
    fn take_removes_entry() {
        let mut t = ReplyTable::new();
        let entry = ReplyEntry::session(token(9));
        let h = t.allocate(entry).expect("room");
        // Tripwire: a handle is one-shot — the first `take` returns
        // the entry and removes it, so a second take (or resolve)
        // against the same handle sees nothing.
        assert_eq!(t.take(h), Some(entry));
        assert_eq!(t.take(h), None);
        assert_eq!(t.resolve(h), None);
    }
}
