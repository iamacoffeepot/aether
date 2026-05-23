//! `aether.trace` cap (ADR-0080 §4). Receives [`BatchedTraceEvents`]
//! from the chassis-owned drainer thread and folds each event into
//! per-root counter maps + a parent → mail graph keyed by `MailId`.
//!
//! PR 2 of issue #707 ships only the state-tracking shape: the
//! observer accumulates `RootState` and `MailNode` entries, applies
//! retention + count-cap eviction, and exposes accessors for tests.
//! `Settled` mail emission is deferred to PR 3 alongside the chassis
//! sentinel + dispatcher switch that routes settlement replies into
//! the gate-site notification map.
//!
//! The observer's own dispatch of `BatchedTraceEvents` does not
//! generate further trace events: the drainer pushes its outbound
//! mail bare through `Mailer::push` (no `Sent` event), and the
//! producer hooks for `Received`/`Finished` short-circuit on
//! `MailId::NONE` (the default for mail not minted via
//! `NativeBinding::send_mail_with_lineage`). See ADR-0080 §7.

use aether_kinds::trace::{
    BatchedTraceEvents, DescribeTree, DescribeTreeResult, DescribeWindow, DescribeWindowResult,
    DispatchTraced, DispatchTracedAck, MailNodeWire, TraceWindow,
};

#[aether_actor::bridge(singleton)]
mod native {
    use super::{
        BatchedTraceEvents, DescribeTree, DescribeTreeResult, DescribeWindow, DescribeWindowResult,
        DispatchTraced, DispatchTracedAck, MailNodeWire, TraceWindow,
    };
    #[cfg(test)]
    use std::collections::HashSet;
    use std::env;
    use std::sync::Arc;
    #[cfg(test)]
    use std::time::{Duration, Instant};

    use rustc_hash::FxHashMap;

    use aether_actor::{MailCtx, actor};
    use aether_data::{KindId, MailId, MailboxId, fnv1a_64_bytes};
    use aether_kinds::trace::{Nanos, Settled, TraceEvent};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::Mail;
    use aether_substrate::mail::helpers::resolve_bundle;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::Registry;

    /// ADR-0080 §11 (amended, iamacoffeepot/aether#1054). The observer
    /// stores mail nodes in a fixed-size ring keyed on ingest sequence;
    /// retention is size-bounded, not time-bounded. `AETHER_TRACE_RING_CAPACITY`
    /// overrides the slot count (rounded up to a power of two so the
    /// `seq % N` slot index is a mask). Memory is a hard ceiling of
    /// `capacity * size_of::<Slot>()` (≈ 28 MB at the default 2^18 ≈ 262k
    /// slots × 112 B), versus the prior soft estimate. The window is "the
    /// last N events"; the ring wraps and overwrites, so there is no
    /// eviction pass.
    const RING_CAPACITY_DEFAULT: usize = 1 << 18;

    /// Slot-`seq` sentinel for a never-written ring slot; real ingest
    /// sequences count up from 0 and never reach this in any realistic
    /// run, so `recycle_slot` skips a slot carrying it.
    const EMPTY_SEQ: u64 = u64::MAX;

    /// `Nanos` sentinel for an unset `t_received` / `t_finished` — a
    /// mail node is written at `Sent` and patched when `Received` /
    /// `Finished` land later. Keeps the slot POD (no `Option<Nanos>`
    /// padding); projected back to `None` at the wire boundary.
    const NANOS_UNSET: Nanos = Nanos(u64::MAX);

    /// Per-root accumulator. `in_flight` tracks how many mails in
    /// this chain are currently between `Sent` and `Finished`;
    /// `held_open` tracks ADR-0080 §12 settlement holds (e.g.
    /// `InheritCtx<A>` from `NativeCtx::spawn_inherit`). `Settled`
    /// emission gates on `(in_flight == 0 && held_open == 0)`, so a
    /// worker thread that outlives its spawning handler keeps the chain
    /// open until it drops.
    ///
    /// A root lives in `roots` iff it has at least one mail slot still
    /// live in the ring (or a pending hold). Overwriting any of its
    /// mails invalidates the whole tree — the root is dropped and its
    /// remaining mails become tombstones (iamacoffeepot/aether#1054).
    #[derive(Debug, Clone)]
    pub struct RootState {
        pub in_flight: u32,
        pub held_open: u32,
    }

    /// One mail node in the ring (iamacoffeepot/aether#1054). Fixed-size
    /// POD (`Copy`, no heap field) so the ring is one contiguous,
    /// alloc-free allocation. `seq` is the observer-assigned ingest
    /// sequence: the slot index is `seq & mask`, and the slot is the
    /// authoritative record for its `seq` only while `head - seq <
    /// capacity`. `t_received` / `t_finished` are [`NANOS_UNSET`] until
    /// the `Received` / `Finished` events patch them. `thread_name_hash`
    /// is `fnv1a_64` of the actor thread name (`0` = none); the name
    /// itself lives in the observer's `thread_names` side table so the
    /// slot stays POD.
    ///
    /// Layout is exactly 112 bytes (all fields 8- or 16-byte, no
    /// padding); the `size_of` assertion below guards against bloat —
    /// confirm codegen with `cargo asm` before adding a field.
    #[derive(Debug, Clone, Copy)]
    struct Slot {
        seq: u64,
        mail_id: MailId,
        root: MailId,
        /// `MailId::NONE` = no parent (root mail).
        parent: MailId,
        sender: MailboxId,
        recipient: MailboxId,
        kind: KindId,
        t_sent: Nanos,
        t_received: Nanos,
        t_finished: Nanos,
        thread_name_hash: u64,
    }

    impl Slot {
        /// A never-written slot; [`EMPTY_SEQ`] makes `recycle_slot` skip
        /// it on the ring's first lap.
        const EMPTY: Self = Self {
            seq: EMPTY_SEQ,
            mail_id: MailId::NONE,
            root: MailId::NONE,
            parent: MailId::NONE,
            sender: MailboxId::NONE,
            recipient: MailboxId::NONE,
            kind: KindId(0),
            t_sent: Nanos(0),
            t_received: NANOS_UNSET,
            t_finished: NANOS_UNSET,
            thread_name_hash: 0,
        };
    }

    const _: () = assert!(
        size_of::<Slot>() == 112,
        "Slot bloated past 112 B — see cargo asm"
    );

    /// `t` projected to the wire `Option<Nanos>` — [`NANOS_UNSET`] → `None`.
    fn nanos_opt(t: Nanos) -> Option<Nanos> {
        (t != NANOS_UNSET).then_some(t)
    }

    /// Per-mail view reconstructed from a [`Slot`] for query replies and
    /// tests. Not stored — the ring is the storage. The owning `root` is
    /// carried by the slot, not this view (it isn't in the wire shape).
    #[derive(Debug, Clone)]
    pub struct MailNode {
        pub parent: Option<MailId>,
        pub sender: MailboxId,
        pub recipient: MailboxId,
        pub kind: KindId,
        pub t_sent: Nanos,
        pub t_received: Option<Nanos>,
        pub t_finished: Option<Nanos>,
        pub thread_name: Option<String>,
    }

    /// `aether.trace` mailbox cap. Folds [`BatchedTraceEvents`] into
    /// per-root counters and a parent → mail graph; emits `Settled`
    /// to [`MailboxId::CHASSIS_MAILBOX_ID`] when a root's `in_flight`
    /// count transitions to zero (ADR-0080 §6).
    pub struct TraceObserverCapability {
        /// Fixed-size ring of mail nodes (iamacoffeepot/aether#1054).
        /// Indexed by `seq & mask`; wraps and overwrites. Pre-allocated,
        /// so steady-state aggregation does no heap work and memory is a
        /// hard ceiling (`capacity * size_of::<Slot>()`).
        ring: Box<[Slot]>,
        /// `capacity - 1`; capacity is a power of two so `seq % capacity`
        /// is the mask `seq & mask`.
        mask: u64,
        /// Next ingest sequence to write; the slot it lands in is
        /// `head & mask`. Strictly monotonic — assigned single-threaded
        /// here — so it orders the ring without depending on the
        /// cross-thread `t_sent` skew.
        head: u64,
        /// `mail_id → seq`, so a later `Received` / `Finished` finds the
        /// originating slot. Verified against `slot.seq`; a stale entry
        /// (slot since recycled) is detected and ignored. Cleaned when
        /// the slot is overwritten.
        /// Fx-hashed (id keys are 64-bit, no denial-of-service surface)
        /// so the hot-path map ops cost a couple multiplies, not the
        /// stdlib default hash.
        by_mail: FxHashMap<MailId, u64>,
        /// Settlement counters per live root. A root is present iff it
        /// still has a live mail in the ring (or a pending hold); see
        /// [`RootState`].
        roots: FxHashMap<MailId, RootState>,
        /// `root → its mail ids`, for `describe_tree` (`O(k)`, not a ring
        /// scan). Dropped wholesale when an overwrite invalidates the tree.
        mails_by_root: FxHashMap<MailId, Vec<MailId>>,
        /// `fnv1a_64(thread name) → name`. The slot stores only the
        /// hash, so names dedup and the slot stays POD.
        thread_names: FxHashMap<u64, String>,
        /// Mailer handle stashed at init so `Settled` mail can be
        /// pushed bare via [`Mailer::push`] — bypassing
        /// `NativeBinding::send_mail_with_lineage` so the outbound
        /// doesn't generate a `TraceEvent::Sent` (which would mint
        /// a fresh `mail_id` chain whose `Finished` never fires —
        /// chassis-router-routed mail doesn't trip the dispatcher's
        /// Received/Finished hooks).
        mailer: Arc<Mailer>,
        /// Cached `Settled` kind id; computed once at init to avoid
        /// re-resolving for every settlement event.
        settled_kind: KindId,
        /// Issue 749: substrate registry handle for `DispatchTraced`'s
        /// per-envelope name resolution (recipient mailbox name → id,
        /// kind name → id). Cloned from `ctx.mailer().registry()` at
        /// init; matches the `RenderCapability` pattern that resolves
        /// `CaptureFrame` mail bundles through the same registry.
        registry: Arc<Registry>,
    }

    impl TraceObserverCapability {
        /// The single struct-construction site. Pre-allocates a ring of
        /// `capacity` slots, rounded up to a power of two so the slot
        /// index is `seq & mask`.
        fn with_capacity(mailer: Arc<Mailer>, registry: Arc<Registry>, capacity: usize) -> Self {
            let capacity = capacity.max(2).next_power_of_two();
            Self {
                ring: vec![Slot::EMPTY; capacity].into_boxed_slice(),
                mask: (capacity - 1) as u64,
                head: 0,
                by_mail: FxHashMap::default(),
                roots: FxHashMap::default(),
                mails_by_root: FxHashMap::default(),
                thread_names: FxHashMap::default(),
                settled_kind: <Settled as aether_data::Kind>::ID,
                mailer,
                registry,
            }
        }

        /// Read-only access to the per-root state map. Used by tests
        /// and by `Settled` consumers; runtime callers should query via
        /// mail rather than reaching across threads.
        #[must_use]
        pub fn roots(&self) -> &FxHashMap<MailId, RootState> {
            &self.roots
        }

        /// Slot index for a sequence. `seq & mask < capacity ≤ usize::MAX`
        /// (capacity is allocated as a `usize`), so the narrowing cast
        /// never truncates.
        #[allow(clippy::cast_possible_truncation)]
        fn slot_index(&self, seq: u64) -> usize {
            (seq & self.mask) as usize
        }

        /// The live slot carrying `seq`, if `seq` is still within the
        /// ring window. A slot is the authoritative record for `seq`
        /// only while it still carries it; once overwritten,
        /// `slot.seq != seq` and the lookup is `None`. Ingest sequences
        /// are globally monotonic and never reused, so there is no ABA.
        fn slot_at(&self, seq: u64) -> Option<&Slot> {
            let slot = &self.ring[self.slot_index(seq)];
            (slot.seq == seq).then_some(slot)
        }

        /// The live slot for a mail id via `by_mail`, `None` if recycled.
        fn slot_for(&self, mail_id: MailId) -> Option<&Slot> {
            self.slot_at(*self.by_mail.get(&mail_id)?)
        }

        /// The live slot at `seq` if its `t_sent` is within `[start, end]`
        /// and its tree is still valid (root present). Drives the
        /// `describe_window` scan; tombstones (root gone) are skipped.
        fn window_slot(&self, seq: u64, start: u64, end: u64) -> Option<&Slot> {
            let slot = self.slot_at(seq)?;
            (slot.t_sent.0 >= start && slot.t_sent.0 <= end && self.roots.contains_key(&slot.root))
                .then_some(slot)
        }

        /// Reconstruct the wire/test view of a slot, resolving the
        /// thread-name hash back to its string and the `Nanos` sentinels
        /// back to `Option`.
        fn node_from_slot(&self, slot: &Slot) -> MailNode {
            MailNode {
                parent: (slot.parent != MailId::NONE).then_some(slot.parent),
                sender: slot.sender,
                recipient: slot.recipient,
                kind: slot.kind,
                t_sent: slot.t_sent,
                t_received: nanos_opt(slot.t_received),
                t_finished: nanos_opt(slot.t_finished),
                thread_name: (slot.thread_name_hash != 0)
                    .then(|| self.thread_names.get(&slot.thread_name_hash).cloned())
                    .flatten(),
            }
        }

        /// Test view of one live, non-tombstone mail node.
        #[cfg(test)]
        fn mail_node(&self, mail_id: MailId) -> Option<MailNode> {
            let slot = self.slot_for(mail_id)?;
            self.roots
                .contains_key(&slot.root)
                .then(|| self.node_from_slot(slot))
        }

        /// Count of live (non-tombstone) mails currently in the ring —
        /// `by_mail` entries whose slot is live and whose tree is still
        /// valid. For test assertions; runtime callers query via mail.
        #[cfg(test)]
        fn live_mail_count(&self) -> usize {
            self.by_mail
                .values()
                .filter_map(|&seq| self.slot_at(seq))
                .filter(|slot| self.roots.contains_key(&slot.root))
                .count()
        }

        fn apply_event(&mut self, event: TraceEvent) {
            match event {
                TraceEvent::Sent {
                    mail_id,
                    root,
                    parent_mail,
                    sender,
                    recipient,
                    kind,
                    t,
                } => {
                    let seq = self.head;
                    let idx = self.slot_index(seq);
                    // Reclaim whatever this slot held (and invalidate its
                    // tree) before overwriting.
                    self.recycle_slot(idx);
                    self.ring[idx] = Slot {
                        seq,
                        mail_id,
                        root,
                        parent: parent_mail.unwrap_or(MailId::NONE),
                        sender,
                        recipient,
                        kind,
                        t_sent: t,
                        t_received: NANOS_UNSET,
                        t_finished: NANOS_UNSET,
                        thread_name_hash: 0,
                    };
                    self.head += 1;
                    self.by_mail.insert(mail_id, seq);
                    self.mails_by_root.entry(root).or_default().push(mail_id);
                    // A `Sent` for an already-settled root re-opens the
                    // chain naturally (in_flight goes 0 → 1); a later
                    // transition back to 0 re-fires `Settled` per the
                    // §6 hint contract. No resurrection bookkeeping —
                    // there is no settlement timestamp to clear.
                    let rs = self.roots.entry(root).or_insert(RootState {
                        in_flight: 0,
                        held_open: 0,
                    });
                    rs.in_flight = rs.in_flight.saturating_add(1);
                }
                TraceEvent::Received {
                    mail_id,
                    t,
                    thread_name,
                } => {
                    // Intern the name before borrowing the slot mutably.
                    let name_hash = thread_name
                        .as_deref()
                        .map_or(0, |n| fnv1a_64_bytes(n.as_bytes()));
                    if name_hash != 0
                        && let Some(name) = thread_name
                    {
                        self.thread_names.entry(name_hash).or_insert(name);
                    }
                    if let Some(&seq) = self.by_mail.get(&mail_id) {
                        let idx = self.slot_index(seq);
                        let slot = &mut self.ring[idx];
                        if slot.seq == seq {
                            slot.t_received = t;
                            if name_hash != 0 {
                                slot.thread_name_hash = name_hash;
                            }
                        }
                    }
                    // Orphan `Received` (no matching live `Sent`) drops.
                    // Eventual-consistency per ADR-0080 §6.
                }
                TraceEvent::Finished { mail_id, t } => {
                    // Patch the slot and recover its root (the event
                    // carries no root). A recycled slot → orphan, dropped.
                    let root = self.by_mail.get(&mail_id).copied().and_then(|seq| {
                        let idx = self.slot_index(seq);
                        let slot = &mut self.ring[idx];
                        (slot.seq == seq).then(|| {
                            slot.t_finished = t;
                            slot.root
                        })
                    });
                    if let Some(root) = root {
                        // ADR-0080 §6 / §12: settlement fires when BOTH
                        // in_flight and held_open reach zero. A tombstoned
                        // tree's root is absent → no decrement, no fire.
                        let settled = self.roots.get_mut(&root).is_some_and(|rs| {
                            rs.in_flight = rs.in_flight.saturating_sub(1);
                            rs.in_flight == 0 && rs.held_open == 0
                        });
                        if settled {
                            self.fire_settled(root);
                        }
                    }
                }
                // ADR-0080 §12 / iamacoffeepot/aether#716: spawn-thread
                // primitives push HoldOpen on acquire and Release on the
                // hold's Drop. HoldOpen may precede the root's Sent under
                // cross-producer reorder, so it `or_insert`s the root to
                // count the hold; such an orphan-hold root carries no live
                // mail and is reaped in `fire_settled`.
                TraceEvent::HoldOpen { root, t: _ } => {
                    let rs = self.roots.entry(root).or_insert(RootState {
                        in_flight: 0,
                        held_open: 0,
                    });
                    rs.held_open = rs.held_open.saturating_add(1);
                }
                TraceEvent::Release { root, t: _ } => {
                    // Symmetric with Finished; only mutates an existing
                    // root. An orphan Release (no HoldOpen, or the tree
                    // was tombstoned) drops. Eventual-consistency §6.
                    let settled = self.roots.get_mut(&root).is_some_and(|rs| {
                        rs.held_open = rs.held_open.saturating_sub(1);
                        rs.in_flight == 0 && rs.held_open == 0
                    });
                    if settled {
                        self.fire_settled(root);
                    }
                }
            }
        }

        /// Reclaim the slot at `idx` before it is overwritten. Drops the
        /// old occupant's `by_mail` entry, then — because losing any mail
        /// leaves a hole in its causal tree — invalidates the whole root:
        /// drops it from `roots` + `mails_by_root` in one step so its
        /// remaining mails become tombstones (skipped by every query
        /// because their root is gone, reclaimed when their own slots
        /// recycle). `O(1)`; the per-write cost that replaces eviction.
        fn recycle_slot(&mut self, idx: usize) {
            let old = self.ring[idx];
            if old.seq == EMPTY_SEQ {
                return;
            }
            if self.by_mail.get(&old.mail_id) == Some(&old.seq) {
                self.by_mail.remove(&old.mail_id);
            }
            // `remove` returns None if a sibling already invalidated this
            // tree (this slot is a tombstone) — nothing more to do.
            // Future (parked, iamacoffeepot/aether#1054): a lapped-but-live
            // tree could be *promoted* to an overflow store keyed by root,
            // retained until it settles and then freed, rather than dropped
            // — preserving completeness for legitimately-slow chains. For
            // now the warn + lifecycle advance-timeout make the drop safe.
            if self.mails_by_root.remove(&old.root).is_some()
                && let Some(rs) = self.roots.remove(&old.root)
                && (rs.in_flight > 0 || rs.held_open > 0)
            {
                tracing::warn!(
                    target: "aether_capabilities::trace",
                    root = ?old.root,
                    in_flight = rs.in_flight,
                    held_open = rs.held_open,
                    "trace ring lapped a live tree — a chain outran the ring window \
                     (a leak, or AETHER_TRACE_RING_CAPACITY too small); dropping it",
                );
            }
        }

        /// Issue 718: pure compute path for `on_describe_tree` —
        /// extracted so tests can exercise filtering without a
        /// `NativeCtx` (the handler is a thin reply wrapper).
        pub(crate) fn build_describe_tree(&self, root: MailId) -> DescribeTreeResult {
            let Some(root_state) = self.roots.get(&root) else {
                return DescribeTreeResult::Err { not_found: root };
            };
            // A live root's mails are all live (any overwrite would have
            // invalidated the whole tree), so this `O(k)` index walk
            // replaces the prior `O(n)` full-`mails` scan.
            let mails: Vec<MailNodeWire> = self
                .mails_by_root
                .get(&root)
                .into_iter()
                .flatten()
                .filter_map(|mid| {
                    let slot = self.slot_for(*mid)?;
                    Some(mail_node_wire_from(*mid, &self.node_from_slot(slot)))
                })
                .collect();
            DescribeTreeResult::Ok {
                root,
                in_flight: root_state.in_flight,
                mails,
            }
        }

        /// Issue 735: pure compute path for `on_describe_window`.
        /// `now` is injected so tests can drive deterministic windows
        /// without depending on `SUBSTRATE_START` being initialised.
        ///
        /// Strict `t_sent` containment: a mail belongs to the window
        /// iff `start_ns <= t_sent <= end_ns`. Counts the matched set
        /// before allocating the reply so an over-cap query returns
        /// `Err { too_many: Some(matched) }` instead of a partial
        /// vector — see issue body for the design rationale.
        pub(crate) fn build_describe_window(
            &self,
            request: DescribeWindow,
            now: Nanos,
        ) -> DescribeWindowResult {
            const DEFAULT_MAX: u32 = 10_000;
            const HARD_MAX: u32 = 100_000;

            let max = request.max_mails.unwrap_or(DEFAULT_MAX).min(HARD_MAX) as usize;

            let (start, end) = match request.window {
                TraceWindow::Absolute { start_ns, end_ns } => {
                    (start_ns, end_ns.unwrap_or(u64::MAX))
                }
                TraceWindow::Relative { last_ms } => {
                    let last_ns = last_ms.saturating_mul(1_000_000);
                    (now.0.saturating_sub(last_ns), now.0)
                }
            };

            // Scan the live ring region (`seq` in `[head - len, head)`).
            // `t_sent` is sorted only to within the cross-thread skew
            // (one-actor-per-thread emit + drainer merge), so a binary
            // search would be heuristic; the region is capacity-bounded
            // and this is a cold query, so an exact linear scan is correct
            // and cheap. Tombstones (root no longer in `roots`) are
            // skipped — a partial tree is never served. Inclusive bounds.
            let len = self.head.min(self.ring.len() as u64);
            let lo = self.head - len;

            // Count first to honour the cap-or-error contract.
            let count = (lo..self.head)
                .filter_map(|seq| self.window_slot(seq, start, end))
                .count();
            if count > max {
                #[allow(clippy::cast_possible_truncation)]
                return DescribeWindowResult::Err {
                    too_many: Some(count as u32),
                };
            }

            let mails: Vec<MailNodeWire> = (lo..self.head)
                .filter_map(|seq| self.window_slot(seq, start, end))
                .map(|slot| mail_node_wire_from(slot.mail_id, &self.node_from_slot(slot)))
                .collect();

            DescribeWindowResult::Ok { mails }
        }

        /// Push `Settled { root }` to `CHASSIS_MAILBOX_ID` via the bare
        /// mailer (so the outbound generates no trace events). An
        /// orphan-hold root — a `HoldOpen` that arrived before, or
        /// without, the root's `Sent` — has no live mail in the ring and
        /// so would never be reclaimed by an overwrite; drop it here once
        /// it settles. A normal root keeps its `roots` entry until its
        /// mails lap, so settled trees stay queryable until they age out.
        fn fire_settled(&mut self, root: MailId) {
            if !self.mails_by_root.contains_key(&root) {
                self.roots.remove(&root);
            }
            let payload = match postcard::to_allocvec(&Settled { root }) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(
                        target: "aether_capabilities::trace",
                        root = ?root,
                        error = %e,
                        "Settled encode failed; chassis subscribers not notified",
                    );
                    return;
                }
            };
            self.mailer.push(Mail::new(
                MailboxId::CHASSIS_MAILBOX_ID,
                self.settled_kind,
                payload,
                1,
            ));
        }
    }

    fn parse_env_usize(name: &str, default: usize) -> usize {
        env::var(name)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    }

    /// Builds the wire-shaped projection of a `MailNode` for the
    /// `describe_tree` / `describe_window` replies. Consolidates the
    /// two near-identical struct-literal sites that differ only in
    /// how the caller obtains the `(mail_id, node)` pair.
    fn mail_node_wire_from(mail_id: MailId, node: &MailNode) -> MailNodeWire {
        MailNodeWire {
            mail_id,
            parent: node.parent,
            sender: node.sender,
            recipient: node.recipient,
            kind: node.kind,
            t_sent: node.t_sent,
            t_received: node.t_received,
            t_finished: node.t_finished,
            thread_name: node.thread_name.clone(),
        }
    }

    #[actor]
    impl NativeActor for TraceObserverCapability {
        type Config = ();
        // ADR-0080 §3 — `aether.trace` (matches
        // `aether_kinds::trace::TRACE_OBSERVER_MAILBOX_NAME`). Has to
        // be a literal here for the `#[actor]` macro's expansion.
        const NAMESPACE: &'static str = "aether.trace";

        fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let capacity = parse_env_usize("AETHER_TRACE_RING_CAPACITY", RING_CAPACITY_DEFAULT);
            let mailer = ctx.mailer();
            let registry = Arc::clone(mailer.registry());
            Ok(Self::with_capacity(mailer, registry, capacity))
        }

        /// ADR-0080 §4 (§11 amended, iamacoffeepot/aether#1054): fold
        /// every event in the batch into the ring + per-root counters.
        /// There is no eviction pass — the ring overwrites in place, so
        /// the per-event hot path is one slot write plus small-map
        /// updates, and memory is a fixed ceiling.
        ///
        /// # Agent
        /// Receives batched trace events from the chassis drainer
        /// thread. Each event is one producer-site emission (`Sent`
        /// at the sender, `Received` at the dispatcher entry,
        /// `Finished` at the dispatcher exit). PR 2 ships state
        /// only; PR 3 wires `Settled` reply emission for gate-site
        /// consumers (lifecycle, frame-loop drain).
        #[handler]
        fn on_batched_trace_events(&mut self, _ctx: &mut NativeCtx<'_>, batch: BatchedTraceEvents) {
            for event in batch.events {
                self.apply_event(event);
            }
        }

        /// # Agent
        /// Returns the mail tree for one root: every node currently in
        /// the observer's `mails` map whose `root` matches the request,
        /// plus the root's current `in_flight` count. Replies
        /// `Err::not_found` when the root isn't tracked (never seen or
        /// evicted past retention). Issue 718 / ADR-0080 Phase 2.
        #[handler]
        fn on_describe_tree(&mut self, ctx: &mut NativeCtx<'_>, request: DescribeTree) {
            let result = self.build_describe_tree(request.root);
            ctx.reply(&result);
        }

        /// # Agent
        /// Returns every mail in the observer whose `t_sent` falls
        /// within the requested window (strict containment). Window
        /// can be absolute nanoseconds or `Relative { last_ms }`
        /// (resolved against the substrate's monotonic now).
        /// `max_mails` caps the reply size (default `10_000`, hard cap
        /// `100_000`); over-cap windows reply `Err { too_many: Some(n)
        /// }` so the caller can narrow the window. Parent edges may
        /// dangle to mail outside the window — drill into a specific
        /// root via `describe_tree` for full chain context. Issue 735.
        #[handler]
        fn on_describe_window(&mut self, ctx: &mut NativeCtx<'_>, request: DescribeWindow) {
            let now = ctx.mailer().now_nanos();
            let result = self.build_describe_window(request, now);
            ctx.reply(&result);
        }

        /// # Agent
        /// Atomic batched dispatch with shared trace root, backing the
        /// MCP `send_mail_traced` tool. Captures this handler's inbound
        /// `MailId` as the batch root, dispatches every spec inheriting
        /// the chain (so all children appear under one tree), and
        /// replies synchronously with [`DispatchTracedAck`] carrying
        /// the root. The caller waits for the wire `ReplyEnd` (chain
        /// settled) and then issues a follow-up [`DescribeTree`] for
        /// the populated tree. Issue 749.
        #[handler]
        fn on_dispatch_traced(&mut self, ctx: &mut NativeCtx<'_>, batch: DispatchTraced) {
            let root = ctx.in_flight_mail_id();
            let DispatchTraced { mails } = batch;
            // Resolve every envelope's name addressing through the
            // substrate registry — same path `CaptureFrame`'s bundle
            // resolution uses (`render::on_capture_frame`). A single
            // unresolved name aborts the whole batch, surfaced as the
            // ack's `Err` variant so the MCP caller fails fast.
            let resolved = match resolve_bundle(&self.registry, &mails, "dispatch_traced batch") {
                Ok(v) => v,
                Err(error) => {
                    ctx.reply(&DispatchTracedAck::Err { error });
                    return;
                }
            };
            for mail in resolved {
                let _ = ctx.send_envelope_traced(mail.recipient, mail.kind, &mail.payload);
            }
            ctx.reply(&DispatchTracedAck::Ok { root });
        }
    }

    #[cfg(test)]
    // Tests hold the capture `Mutex` guard across the assertion block
    // so the snapshot reads atomically against the concurrent
    // observer-side push.
    #[allow(clippy::significant_drop_tightening)]
    mod tests {
        use super::*;
        use aether_data::{SessionToken, Uuid};
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
        use aether_substrate::mail::registry::{MailDispatch, Registry};
        use aether_substrate::mail::{ReplyTarget, ReplyTo};
        use std::sync::Mutex;
        use std::sync::mpsc::Receiver;

        /// Construct an observer for state-fold tests. Stash a fresh
        /// `Mailer` so `fire_settled` has somewhere to push (the
        /// chassis-router isn't installed, so the bare push warn-drops
        /// at the `route_mail` switch — that's fine for state assertions).
        fn boot_observer() -> TraceObserverCapability {
            observer_with(1024)
        }

        fn observer_with(capacity: usize) -> TraceObserverCapability {
            let registry = Arc::new(Registry::new());
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
            TraceObserverCapability::with_capacity(mailer, registry, capacity)
        }

        fn mail(sender: u64, cid: u64) -> MailId {
            MailId {
                sender: MailboxId(sender),
                correlation_id: cid,
            }
        }

        /// Consolidates the `obs.apply_event(TraceEvent::Sent { ... })`
        /// call-site that recurs across every state-fold test. Same
        /// field order as the variant so call sites read positionally.
        #[allow(clippy::too_many_arguments)]
        fn apply_sent_event(
            obs: &mut TraceObserverCapability,
            mail_id: MailId,
            root: MailId,
            parent_mail: Option<MailId>,
            sender: MailboxId,
            recipient: MailboxId,
            kind: KindId,
            t: Nanos,
        ) {
            obs.apply_event(TraceEvent::Sent {
                mail_id,
                root,
                parent_mail,
                sender,
                recipient,
                kind,
                t,
            });
        }

        #[test]
        fn sent_creates_root_and_node() {
            let mut obs = boot_observer();
            let m = mail(1, 1);
            apply_sent_event(
                &mut obs,
                m,
                m,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            assert_eq!(obs.roots.len(), 1);
            assert_eq!(
                obs.roots
                    .get(&m)
                    .expect("root entry exists for mail")
                    .in_flight,
                1
            );
            assert_eq!(obs.live_mail_count(), 1);
            assert_eq!(
                obs.mail_node(m).expect("mail node exists for mail").t_sent,
                Nanos(100)
            );
        }

        #[test]
        fn child_inherits_root_via_parent_mail() {
            let mut obs = boot_observer();
            let root = mail(1, 1);
            let child = mail(2, 1);
            apply_sent_event(
                &mut obs,
                root,
                root,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            apply_sent_event(
                &mut obs,
                child,
                root,
                Some(root),
                MailboxId(2),
                MailboxId(3),
                KindId(0xCDEF),
                Nanos(200),
            );
            assert_eq!(obs.roots.len(), 1);
            assert_eq!(
                obs.roots
                    .get(&root)
                    .expect("root entry exists for root mail")
                    .in_flight,
                2
            );
            assert_eq!(
                obs.mail_node(child).expect("child mail node exists").parent,
                Some(root)
            );
        }

        #[test]
        fn finished_decrements_root_in_flight() {
            let mut obs = boot_observer();
            let m = mail(1, 1);
            apply_sent_event(
                &mut obs,
                m,
                m,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            obs.apply_event(TraceEvent::Received {
                mail_id: m,
                t: Nanos(200),
                thread_name: Some("aether-root-test".to_owned()),
            });
            obs.apply_event(TraceEvent::Finished {
                mail_id: m,
                t: Nanos(300),
            });
            assert_eq!(
                obs.roots
                    .get(&m)
                    .expect("root entry exists for mail")
                    .in_flight,
                0
            );
            let node = obs.mail_node(m).expect("mail node exists for mail");
            assert_eq!(node.t_received, Some(Nanos(200)));
            assert_eq!(node.t_finished, Some(Nanos(300)));
            assert_eq!(node.thread_name.as_deref(), Some("aether-root-test"));
        }

        /// Timing/soak measurement (iamacoffeepot/aether#1054). Not a CI
        /// gate — run explicitly:
        /// `cargo test -p aether-capabilities trace_observer_throughput
        ///  -- --ignored --nocapture`. Prints fold throughput at a
        /// steady-state population and the cost of draining the whole
        /// settled set in one eviction pass (the iamacoffeepot/aether#1048
        /// spike shape).
        #[test]
        #[ignore = "timing/soak measurement; run with --ignored --nocapture"]
        #[allow(clippy::print_stdout, clippy::cast_precision_loss)]
        fn trace_observer_throughput_and_cleanup() {
            fn pump_tick(obs: &mut TraceObserverCapability, n: u64, t0: u64) {
                let root = mail(0xC0DE, n);
                let child = mail(0xBEEF, n);
                apply_sent_event(
                    obs,
                    root,
                    root,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xA),
                    Nanos(t0),
                );
                apply_sent_event(
                    obs,
                    child,
                    root,
                    Some(root),
                    MailboxId(2),
                    MailboxId(3),
                    KindId(0xB),
                    Nanos(t0 + 1),
                );
                obs.apply_event(TraceEvent::Received {
                    mail_id: child,
                    t: Nanos(t0 + 2),
                    thread_name: Some("aether-actor-0".to_owned()),
                });
                obs.apply_event(TraceEvent::Finished {
                    mail_id: child,
                    t: Nanos(t0 + 3),
                });
                obs.apply_event(TraceEvent::Finished {
                    mail_id: root,
                    t: Nanos(t0 + 4),
                });
            }

            const CAP: usize = 1 << 17;
            const POP: u64 = 60_000;
            const MEASURE: u64 = 40_000;

            let mut obs = observer_with(CAP);
            for i in 0..POP {
                pump_tick(&mut obs, i, i * 8);
            }
            println!(
                "FILLED: {} live roots / {} live mails (ring cap {CAP})",
                obs.roots.len(),
                obs.live_mail_count()
            );

            // Steady state: every tick now wraps the ring, overwriting old
            // slots and invalidating their trees in `recycle_slot` — the
            // realistic hot path, with cleanup folded into the fold.
            let start = Instant::now();
            for i in POP..(POP + MEASURE) {
                pump_tick(&mut obs, i, i * 8);
            }
            let fold = start.elapsed();
            let events = MEASURE * 5;
            println!(
                "FOLD (steady-state, wrapping): {events} events in {fold:?} = {:.1} ns/event",
                fold.as_nanos() as f64 / events as f64
            );

            // Hard memory ceiling: live roots + forward index never exceed
            // the ring capacity, no matter how many trees flowed through.
            assert!(
                obs.roots.len() <= CAP && obs.by_mail.len() <= CAP,
                "ring memory not bounded: {} roots / {} by_mail vs cap {CAP}",
                obs.roots.len(),
                obs.by_mail.len(),
            );
            println!(
                "BOUNDED: {} live roots / {} by_mail entries (≤ cap {CAP})",
                obs.roots.len(),
                obs.by_mail.len()
            );
        }

        #[test]
        fn orphan_received_drops_silently() {
            let mut obs = boot_observer();
            let m = mail(1, 1);
            obs.apply_event(TraceEvent::Received {
                mail_id: m,
                t: Nanos(100),
                thread_name: None,
            });
            assert_eq!(obs.live_mail_count(), 0);
            assert!(obs.roots.is_empty());
        }

        /// iamacoffeepot/aether#1054: the ring overwrites the oldest slot
        /// when full, invalidating that tree. Capacity 4 + six single-mail
        /// roots → the two oldest (cid 1, 2) are overwritten and gone; the
        /// four newest survive. No eviction pass — wrapping is the GC.
        #[test]
        fn ring_overwrites_oldest_when_full() {
            let mut obs = observer_with(4);
            for cid in 1..=6 {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(cid * 100),
                );
            }
            assert_eq!(obs.roots.len(), 4, "ring holds at most capacity trees");
            assert!(!obs.roots.contains_key(&mail(1, 1)), "oldest overwritten");
            assert!(
                !obs.roots.contains_key(&mail(1, 2)),
                "second-oldest overwritten"
            );
            for cid in 3..=6 {
                assert!(obs.roots.contains_key(&mail(1, cid)), "recent survives");
            }
            assert_eq!(obs.live_mail_count(), 4);
        }

        /// iamacoffeepot/aether#1054: overwriting *any* mail of a tree
        /// invalidates the whole tree — the root drops and its remaining
        /// mails become tombstones, skipped by queries. Capacity 4, a
        /// 3-mail tree then enough singles to lap the tree's root slot:
        /// `describe_tree` then reports the tree gone even though sibling
        /// slots are still physically present.
        #[test]
        fn overwriting_one_mail_invalidates_whole_tree() {
            let mut obs = observer_with(4);
            let root = mail(1, 1);
            let a = mail(2, 1);
            let b = mail(2, 2);
            // Tree: root (seq 0), a (seq 1), b (seq 2).
            apply_sent_event(
                &mut obs,
                root,
                root,
                None,
                MailboxId(1),
                MailboxId(9),
                KindId(1),
                Nanos(10),
            );
            apply_sent_event(
                &mut obs,
                a,
                root,
                Some(root),
                MailboxId(9),
                MailboxId(8),
                KindId(2),
                Nanos(20),
            );
            apply_sent_event(
                &mut obs,
                b,
                root,
                Some(root),
                MailboxId(9),
                MailboxId(7),
                KindId(3),
                Nanos(30),
            );
            assert!(matches!(
                obs.build_describe_tree(root),
                DescribeTreeResult::Ok { .. }
            ));
            // Two more singles (seq 3, 4): seq 4 wraps to idx 0, overwriting
            // the tree's root mail (seq 0) → the whole tree is invalidated.
            for cid in 10..=11 {
                let m = mail(5, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(5),
                    MailboxId(6),
                    KindId(4),
                    Nanos(cid * 100),
                );
            }
            assert_eq!(
                obs.build_describe_tree(root),
                DescribeTreeResult::Err { not_found: root },
                "tree with an overwritten mail is reported gone, not partial",
            );
            assert!(!obs.roots.contains_key(&root));
        }

        #[test]
        fn finished_to_zero_fires_settled() {
            let (mut obs, captured, root) = observer_with_settled_capture();
            apply_sent_event(
                &mut obs,
                root,
                root,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            // No Settled yet — in_flight is 1.
            assert!(captured.lock().expect("captured mutex").is_empty());
            obs.apply_event(TraceEvent::Finished {
                mail_id: root,
                t: Nanos(200),
            });
            // Settled fired; chassis-router decoded the mail.
            let captured = captured.lock().expect("captured mutex");
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].root, root);
        }

        /// ADR-0080 §12 / iamacoffeepot/aether#716 — shared fixture
        /// for settlement-hold tests. Returns the observer +
        /// `Mutex<Vec<Settled>>` capture (chassis-router-routed) +
        /// the chassis-root `MailId` so the tests can apply events
        /// against the same root the capture sees.
        fn observer_with_settled_capture()
        -> (TraceObserverCapability, Arc<Mutex<Vec<Settled>>>, MailId) {
            let registry = Arc::new(Registry::new());
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
            let captured: Arc<Mutex<Vec<Settled>>> = Arc::new(Mutex::new(Vec::new()));
            let captured_for_router = Arc::clone(&captured);
            let settled_kind = <Settled as aether_data::Kind>::ID;
            mailer.install_chassis_router(Box::new(move |mail| {
                if mail.kind == settled_kind
                    && let Ok(notice) = postcard::from_bytes::<Settled>(&mail.payload)
                {
                    captured_for_router
                        .lock()
                        .expect("test stub: captured mutex poisoned")
                        .push(notice);
                }
            }));

            let obs = TraceObserverCapability::with_capacity(
                Arc::clone(&mailer),
                Arc::clone(&registry),
                1024,
            );
            (obs, captured, mail(1, 1))
        }

        /// Spawn completes before handler returns: `HoldOpen` +
        /// `Release` land before `Finished`. Settlement fires when
        /// `Finished` drops `in_flight` to 0 because `held_open` is
        /// already 0 by then.
        #[test]
        fn hold_acquired_then_released_before_finished() {
            let (mut obs, captured, root) = observer_with_settled_capture();

            apply_sent_event(
                &mut obs,
                root,
                root,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            obs.apply_event(TraceEvent::HoldOpen {
                root,
                t: Nanos(150),
            });
            obs.apply_event(TraceEvent::Release {
                root,
                t: Nanos(160),
            });
            // in_flight=1, held_open=0 — Release on its own doesn't fire.
            assert!(
                captured.lock().expect("captured mutex").is_empty(),
                "Release with in_flight > 0 must not fire Settled"
            );

            obs.apply_event(TraceEvent::Finished {
                mail_id: root,
                t: Nanos(200),
            });
            let captured = captured.lock().expect("captured mutex");
            assert_eq!(captured.len(), 1, "Finished with held_open=0 fires");
            assert_eq!(captured[0].root, root);
        }

        /// The iamacoffeepot/aether#716 bug: spawn outlives handler.
        /// `Finished` fires before `Release`. With the gate the
        /// `Finished` event must NOT trigger `Settled`; only the
        /// subsequent `Release` (which drops `held_open` to 0 while
        /// `in_flight` is already 0) fires settlement.
        #[test]
        fn hold_outlives_finished_blocks_then_release_fires_settled() {
            let (mut obs, captured, root) = observer_with_settled_capture();

            apply_sent_event(
                &mut obs,
                root,
                root,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            obs.apply_event(TraceEvent::HoldOpen {
                root,
                t: Nanos(150),
            });
            obs.apply_event(TraceEvent::Finished {
                mail_id: root,
                t: Nanos(200),
            });

            // The bug: in_flight=0 but held_open=1, so Settled must NOT
            // have fired yet. Pre-fix this assertion would fail (Settled
            // would have been captured).
            assert!(
                captured.lock().expect("captured mutex").is_empty(),
                "Settled must be gated by held_open > 0"
            );

            // Worker thread "drops" — Release event arrives.
            obs.apply_event(TraceEvent::Release {
                root,
                t: Nanos(300),
            });
            let captured = captured.lock().expect("captured mutex");
            assert_eq!(
                captured.len(),
                1,
                "Release with in_flight=0 fires the previously-gated Settled"
            );
            assert_eq!(captured[0].root, root);
        }

        /// Multiple concurrent spawns: each `InheritCtx` acquires its
        /// own hold; the observer accumulates `held_open` to N. Only
        /// the last Release brings both counters to zero, firing
        /// exactly one Settled.
        #[test]
        fn multiple_holds_each_gate_settlement() {
            let (mut obs, captured, root) = observer_with_settled_capture();

            apply_sent_event(
                &mut obs,
                root,
                root,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            obs.apply_event(TraceEvent::HoldOpen {
                root,
                t: Nanos(110),
            });
            obs.apply_event(TraceEvent::HoldOpen {
                root,
                t: Nanos(120),
            });
            obs.apply_event(TraceEvent::HoldOpen {
                root,
                t: Nanos(130),
            });
            assert_eq!(
                obs.roots.get(&root).expect("root").held_open,
                3,
                "three holds → counter at 3"
            );

            obs.apply_event(TraceEvent::Finished {
                mail_id: root,
                t: Nanos(200),
            });
            obs.apply_event(TraceEvent::Release {
                root,
                t: Nanos(210),
            });
            obs.apply_event(TraceEvent::Release {
                root,
                t: Nanos(220),
            });
            // Two of three holds released; held_open=1, in_flight=0.
            assert!(
                captured.lock().expect("captured mutex").is_empty(),
                "partial release does not fire Settled"
            );

            obs.apply_event(TraceEvent::Release {
                root,
                t: Nanos(230),
            });
            let captured = captured.lock().expect("captured mutex");
            assert_eq!(captured.len(), 1, "last Release fires exactly one Settled");
            assert_eq!(captured[0].root, root);
        }

        /// `HoldOpen` for a never-seen root creates the `RootState`
        /// entry with `in_flight = 0`. A subsequent `Sent` ticks
        /// `in_flight` up alongside the existing `held_open`. Order
        /// `Sent`-before-`HoldOpen` vs `HoldOpen`-before-`Sent` is
        /// producer-order-sensitive in practice but the observer is
        /// symmetric.
        #[test]
        fn hold_open_creates_root_state_with_zero_in_flight() {
            let mut obs = boot_observer();
            let root = mail(1, 1);
            obs.apply_event(TraceEvent::HoldOpen { root, t: Nanos(50) });
            let state = obs.roots.get(&root).expect("HoldOpen creates root");
            assert_eq!(state.in_flight, 0);
            assert_eq!(state.held_open, 1);
        }

        /// Orphan `Release` (no matching `HoldOpen` — chassis didn't
        /// have the trace queue installed when the hold was acquired,
        /// or the root was evicted) drops silently. No state change,
        /// no panic.
        #[test]
        fn orphan_release_drops_silently() {
            let mut obs = boot_observer();
            let root = mail(7, 7);
            obs.apply_event(TraceEvent::Release {
                root,
                t: Nanos(100),
            });
            assert!(obs.roots.is_empty(), "orphan Release does not create state");
            assert_eq!(obs.live_mail_count(), 0);
        }

        #[test]
        fn describe_tree_returns_full_subtree() {
            let mut obs = boot_observer();
            let root = mail(1, 1);
            let a = mail(2, 1);
            let b = mail(2, 2);
            let unrelated = mail(9, 9);
            apply_sent_event(
                &mut obs,
                root,
                root,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            apply_sent_event(
                &mut obs,
                a,
                root,
                Some(root),
                MailboxId(2),
                MailboxId(3),
                KindId(0xCDEF),
                Nanos(200),
            );
            apply_sent_event(
                &mut obs,
                b,
                root,
                Some(root),
                MailboxId(2),
                MailboxId(4),
                KindId(0xDEAD),
                Nanos(300),
            );
            apply_sent_event(
                &mut obs,
                unrelated,
                unrelated,
                None,
                MailboxId(9),
                MailboxId(8),
                KindId(0xBEEF),
                Nanos(400),
            );

            let result = obs.build_describe_tree(root);
            match result {
                DescribeTreeResult::Ok {
                    root: r,
                    in_flight,
                    mails,
                } => {
                    assert_eq!(r, root);
                    assert_eq!(in_flight, 3);
                    assert_eq!(mails.len(), 3);
                    let ids: HashSet<MailId> = mails.iter().map(|m| m.mail_id).collect();
                    assert!(ids.contains(&root));
                    assert!(ids.contains(&a));
                    assert!(ids.contains(&b));
                    assert!(!ids.contains(&unrelated));
                }
                DescribeTreeResult::Err { not_found } => {
                    panic!("expected Ok, got Err::not_found {not_found:?}")
                }
            }
        }

        #[test]
        fn describe_tree_unknown_root_returns_err() {
            let obs = boot_observer();
            let missing = mail(7, 7);
            assert_eq!(
                obs.build_describe_tree(missing),
                DescribeTreeResult::Err { not_found: missing }
            );
        }

        #[test]
        fn describe_window_returns_in_window_mails() {
            let mut obs = boot_observer();
            // Three sends at t = 100, 500, 900.
            for (cid, t) in [(1u64, 100u64), (2, 500), (3, 900)] {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(t),
                );
            }
            // Window [200, 800] strictly contains only the t=500 mail.
            let result = obs.build_describe_window(
                DescribeWindow {
                    window: TraceWindow::Absolute {
                        start_ns: 200,
                        end_ns: Some(800),
                    },
                    max_mails: None,
                },
                Nanos(1_000),
            );
            match result {
                DescribeWindowResult::Ok { mails } => {
                    assert_eq!(mails.len(), 1);
                    assert_eq!(mails[0].mail_id, mail(1, 2));
                }
                DescribeWindowResult::Err { too_many } => {
                    panic!("expected Ok, got Err::too_many {too_many:?}")
                }
            }
        }

        #[test]
        fn describe_window_inclusive_at_boundaries() {
            let mut obs = boot_observer();
            for (cid, t) in [(1u64, 200u64), (2, 800)] {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(t),
                );
            }
            let result = obs.build_describe_window(
                DescribeWindow {
                    window: TraceWindow::Absolute {
                        start_ns: 200,
                        end_ns: Some(800),
                    },
                    max_mails: None,
                },
                Nanos(1_000),
            );
            match result {
                DescribeWindowResult::Ok { mails } => assert_eq!(mails.len(), 2),
                DescribeWindowResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn describe_window_collisions_at_same_t_sent() {
            // Several mails sharing one `Nanos` all fall in an inclusive
            // [t, t] window — the linear scan needs no tie-break.
            let mut obs = boot_observer();
            for cid in 1..=3u64 {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(500),
                );
            }
            let result = obs.build_describe_window(
                DescribeWindow {
                    window: TraceWindow::Absolute {
                        start_ns: 500,
                        end_ns: Some(500),
                    },
                    max_mails: None,
                },
                Nanos(1_000),
            );
            match result {
                DescribeWindowResult::Ok { mails } => assert_eq!(mails.len(), 3),
                DescribeWindowResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn describe_window_too_many_returns_err() {
            let mut obs = boot_observer();
            for cid in 1..=10u64 {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(cid * 10),
                );
            }
            let result = obs.build_describe_window(
                DescribeWindow {
                    window: TraceWindow::Absolute {
                        start_ns: 0,
                        end_ns: Some(1_000),
                    },
                    max_mails: Some(5),
                },
                Nanos(1_000),
            );
            assert_eq!(result, DescribeWindowResult::Err { too_many: Some(10) });
        }

        #[test]
        fn describe_window_relative_resolves_against_now() {
            let mut obs = boot_observer();
            // Sends at t = 1s, 5s, 10s (in nanos).
            for (cid, t) in [
                (1u64, 1_000_000_000u64),
                (2, 5_000_000_000),
                (3, 10_000_000_000),
            ] {
                let m = mail(1, cid);
                apply_sent_event(
                    &mut obs,
                    m,
                    m,
                    None,
                    MailboxId(1),
                    MailboxId(2),
                    KindId(0xABCD),
                    Nanos(t),
                );
            }
            // now = 11s, last_ms = 6_000 → window [5s, 11s] keeps cids 2, 3.
            let result = obs.build_describe_window(
                DescribeWindow {
                    window: TraceWindow::Relative { last_ms: 6_000 },
                    max_mails: None,
                },
                Nanos(11_000_000_000),
            );
            match result {
                DescribeWindowResult::Ok { mails } => {
                    assert_eq!(mails.len(), 2);
                    let ids: HashSet<MailId> = mails.iter().map(|m| m.mail_id).collect();
                    assert!(ids.contains(&mail(1, 2)));
                    assert!(ids.contains(&mail(1, 3)));
                }
                DescribeWindowResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn describe_window_empty_when_no_match() {
            let mut obs = boot_observer();
            let m = mail(1, 1);
            apply_sent_event(
                &mut obs,
                m,
                m,
                None,
                MailboxId(1),
                MailboxId(2),
                KindId(0xABCD),
                Nanos(100),
            );
            let result = obs.build_describe_window(
                DescribeWindow {
                    window: TraceWindow::Absolute {
                        start_ns: 1_000,
                        end_ns: Some(2_000),
                    },
                    max_mails: None,
                },
                Nanos(3_000),
            );
            match result {
                DescribeWindowResult::Ok { mails } => assert!(mails.is_empty()),
                DescribeWindowResult::Err { .. } => panic!("expected Ok"),
            }
        }

        /// iamacoffeepot/aether#1054 regression: under the wedge-causing
        /// load shape (a steady stream of settle-then-age-out trees), the
        /// ring keeps the working set hard-bounded by capacity with no
        /// eviction pass — overwriting is the GC. The prior design grew
        /// `roots` / `mails` without bound for minutes (10-min retention)
        /// until the per-batch drain spiked and settlement latency
        /// exceeded the frame tick (#1048). Here many batches of
        /// advance-shaped trees flow through a small ring; the live set
        /// never exceeds capacity and the indexes stay consistent.
        #[test]
        fn soak_ring_stays_bounded_no_eviction_pass() {
            const CAP: usize = 256;
            const BATCHES: usize = 200;
            const PER_BATCH: usize = 50;

            let mut obs = observer_with(CAP);
            let mut max_roots_seen = 0usize;
            let mut max_by_mail_seen = 0usize;
            let mut total_settled = 0usize;

            for b in 0..BATCHES {
                for i in 0..PER_BATCH {
                    let cid = (b * PER_BATCH + i + 1) as u64;
                    let m = mail(1, cid);
                    // Advance-shaped: Sent then Finished — the tree settles.
                    apply_sent_event(
                        &mut obs,
                        m,
                        m,
                        None,
                        MailboxId(1),
                        MailboxId(2),
                        KindId(0xABCD),
                        Nanos(cid),
                    );
                    obs.apply_event(TraceEvent::Finished {
                        mail_id: m,
                        t: Nanos(cid),
                    });
                    total_settled += 1;
                }
                max_roots_seen = max_roots_seen.max(obs.roots.len());
                max_by_mail_seen = max_by_mail_seen.max(obs.by_mail.len());
            }

            let cumulative = BATCHES * PER_BATCH;
            assert_eq!(total_settled, cumulative);
            assert!(
                cumulative > CAP * 4,
                "soak must lap the ring many times to be meaningful"
            );
            // The live set is hard-bounded by the ring — it never grows
            // with cumulative inflow (which the old design did).
            assert!(
                max_roots_seen <= CAP,
                "live roots {max_roots_seen} exceeded ring capacity {CAP}"
            );
            assert!(
                max_by_mail_seen <= CAP,
                "by_mail {max_by_mail_seen} exceeded ring capacity {CAP}"
            );
            // Index consistency: every by_mail entry points at its live
            // slot (recycle cleans stale entries), and every live root
            // either has a mails_by_root list or an outstanding hold.
            for (&mid, &seq) in &obs.by_mail {
                let slot = obs.slot_at(seq).expect("by_mail points at a live slot");
                assert_eq!(slot.mail_id, mid, "by_mail points at the wrong slot");
            }
            for root in obs.roots.keys() {
                assert!(
                    obs.mails_by_root.contains_key(root)
                        || obs.roots.get(root).is_some_and(|r| r.held_open > 0),
                    "live root {root:?} has neither a mails_by_root entry nor a hold"
                );
            }
        }

        /// Shared scaffolding for the `on_dispatch_traced` tests:
        /// fresh registry + mailer + outbound + transport + observer
        /// wired together with the `mailer.outbound -> rx` egress
        /// recorder. The observer doesn't go through `init` (which
        /// reads env vars and ctx state); construct it directly with
        /// the registry handle the resolve path needs.
        struct DispatchTracedFixture {
            registry: Arc<Registry>,
            rx: Receiver<EgressEvent>,
            transport: Arc<NativeBinding>,
            cap: TraceObserverCapability,
        }

        fn dispatch_traced_fixture() -> DispatchTracedFixture {
            let registry = Arc::new(Registry::new());
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let (outbound, rx) = HubOutbound::attached_loopback();
            let mailer = Arc::new(
                Mailer::new(Arc::clone(&registry), Arc::clone(&store)).with_outbound(outbound),
            );
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0x7ACE),
            ));
            let cap = TraceObserverCapability::with_capacity(mailer, Arc::clone(&registry), 1024);
            DispatchTracedFixture {
                registry,
                rx,
                transport,
                cap,
            }
        }

        /// Build a chassis-root `NativeCtx` against the fixture's
        /// transport, anchoring the in-flight + reply-to fields to a
        /// session sender so the ack reply egresses as `ToSession`.
        fn chassis_root_ctx(transport: &Arc<NativeBinding>, inbound: MailId) -> NativeCtx<'_> {
            let sender = ReplyTo::to(ReplyTarget::Session(SessionToken(Uuid::nil())));
            NativeCtx::new(transport, sender, inbound, inbound)
        }

        /// Drain `rx` until it goes quiet, decoding every
        /// `DispatchTracedAck` `ToSession` egress it sees. Exactly
        /// one ack is the success shape; the test asserts on `len()`.
        fn drain_ack_replies(rx: &Receiver<EgressEvent>) -> Vec<DispatchTracedAck> {
            let mut acks = Vec::new();
            while let Ok(event) = rx.recv_timeout(Duration::from_millis(250)) {
                if let EgressEvent::ToSession {
                    kind_name, payload, ..
                } = event
                    && kind_name == <DispatchTracedAck as aether_data::Kind>::NAME
                {
                    acks.push(postcard::from_bytes(&payload).expect("ack payload decodes"));
                }
            }
            acks
        }

        /// Issue 749: `on_dispatch_traced` resolves each envelope's
        /// name addressing through the registry (matching
        /// `CaptureFrame`'s bundle pattern), dispatches each via
        /// `send_envelope_traced` so children inherit the chain, and
        /// replies synchronously with `DispatchTracedAck::Ok { root }`
        /// carrying the inbound mail id.
        #[test]
        fn on_dispatch_traced_resolves_each_envelope_and_acks_with_root() {
            use aether_kinds::MailEnvelope;
            use std::sync::Mutex;

            type Capture = (KindId, MailId, Option<MailId>, Vec<u8>);

            /// Inline handler that records every dispatched mail's
            /// `(kind, root, parent, payload)` into the shared
            /// `Vec`. Used twice to register two stub recipients.
            fn register_capture(registry: &Registry, name: &str, sink: Arc<Mutex<Vec<Capture>>>) {
                registry.register_inline(
                    name,
                    Arc::new(move |d: MailDispatch<'_>| {
                        sink.lock()
                            .expect("test stub: captured mutex poisoned")
                            .push((d.kind, d.root, d.parent_mail, d.payload.to_vec()));
                    }),
                );
            }

            let mut fix = dispatch_traced_fixture();
            // Resolve_bundle needs both mailbox (by name) and kind to
            // be registered, else it short-circuits with the early-
            // abort `Err` path the other test exercises.
            let captured: Arc<Mutex<Vec<Capture>>> = Arc::new(Mutex::new(Vec::new()));
            register_capture(&fix.registry, "aether.test.spec_a", Arc::clone(&captured));
            register_capture(&fix.registry, "aether.test.spec_b", Arc::clone(&captured));
            let kind_alpha = fix.registry.register_kind("aether.test.kind_a");
            let kind_beta = fix.registry.register_kind("aether.test.kind_b");

            let inbound = MailId::new(MailboxId(0xC0DE), 7);
            let mut ctx = chassis_root_ctx(&fix.transport, inbound);
            fix.cap.on_dispatch_traced(
                &mut ctx,
                DispatchTraced {
                    mails: vec![
                        MailEnvelope {
                            recipient_name: "aether.test.spec_a".into(),
                            kind_name: "aether.test.kind_a".into(),
                            payload: vec![1u8, 2],
                            count: 1,
                        },
                        MailEnvelope {
                            recipient_name: "aether.test.spec_b".into(),
                            kind_name: "aether.test.kind_b".into(),
                            payload: vec![3u8, 4, 5],
                            count: 1,
                        },
                    ],
                },
            );

            let snapshot = captured
                .lock()
                .expect("test stub: captured mutex poisoned")
                .clone();
            assert_eq!(snapshot.len(), 2, "expected each envelope to dispatch");
            assert!(
                snapshot.iter().any(|(k, root, parent, p)| *k == kind_alpha
                    && *root == inbound
                    && *parent == Some(inbound)
                    && p == &vec![1u8, 2]),
                "envelope A missing or chain not inherited; captured: {snapshot:?}"
            );
            assert!(
                snapshot.iter().any(|(k, root, parent, p)| *k == kind_beta
                    && *root == inbound
                    && *parent == Some(inbound)
                    && p == &vec![3u8, 4, 5]),
                "envelope B missing or chain not inherited; captured: {snapshot:?}"
            );

            let acks = drain_ack_replies(&fix.rx);
            assert_eq!(acks.len(), 1, "expected exactly one ack reply");
            match &acks[0] {
                DispatchTracedAck::Ok { root } => assert_eq!(
                    *root, inbound,
                    "Ok ack must echo the in-flight inbound mail id as the chassis root"
                ),
                DispatchTracedAck::Err { error } => {
                    panic!("expected Ok ack, got Err: {error}")
                }
            }
        }

        /// Issue 749: an unresolvable name in the batch short-circuits
        /// to `DispatchTracedAck::Err`; no envelope dispatches.
        #[test]
        fn on_dispatch_traced_replies_err_on_unknown_recipient() {
            use aether_kinds::MailEnvelope;

            let mut fix = dispatch_traced_fixture();
            let inbound = MailId::new(MailboxId(0xC0DE), 99);
            let mut ctx = chassis_root_ctx(&fix.transport, inbound);
            fix.cap.on_dispatch_traced(
                &mut ctx,
                DispatchTraced {
                    mails: vec![MailEnvelope {
                        recipient_name: "aether.test.does_not_exist".into(),
                        kind_name: "aether.test.also_missing".into(),
                        payload: vec![],
                        count: 1,
                    }],
                },
            );

            let acks = drain_ack_replies(&fix.rx);
            assert_eq!(acks.len(), 1);
            assert!(
                matches!(&acks[0], DispatchTracedAck::Err { error } if error.contains("unknown recipient")),
                "expected Err with 'unknown recipient' message, got: {:?}",
                acks[0]
            );
        }
    }
}
