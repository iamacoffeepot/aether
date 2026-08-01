//! The public registration surface: claiming a mailbox name for an inbox
//! or inline handler, retiring it, and installing a pooled actor's seize
//! handle behind it.

use std::sync::Arc;

use crate::mail::MailboxId;
use crate::mail::registry::authority::BootAuthority;
use crate::mail::registry::effect::{RegistryApplied, RegistryEffect, RegistryEffectError};
use crate::mail::registry::errors::{DropError, NameConflict};
use crate::mail::registry::handlers::{InboxHandler, InlineHandler};
use crate::scheduler::SeizeHandle;

use super::{MailboxEntry, Registry, SeizeCell};

impl Registry {
    /// Insert a mailbox, allocating its id from the name hash (ADR-0029).
    /// On a `Dropped` entry at the same id (same name re-registered
    /// after a drop), the entry transitions back to live. Any other
    /// occupied entry is a collision.
    fn insert(&self, authority: &BootAuthority, name: String, entry: MailboxEntry) -> Result<MailboxId, NameConflict> {
        // Depth-1 / root registrations derive the id from the name
        // (ADR-0029) — the lineage fold's fixed point.
        self.insert_with_id(authority, MailboxId::from_name(&name), name, entry)
    }

    /// ADR-0099 §3: register under an explicit, caller-computed `id`
    /// (the lineage fold) with `name` retained as the display /
    /// reverse-map string. `MailboxId = hash(name)` no longer holds for
    /// a hosted / spawned actor — its id is the fold over its lineage of
    /// `ActorId`s — so the spawn path passes the folded id here instead
    /// of letting the name derive it. [`Self::insert`] is the depth-1
    /// case where the two coincide.
    fn insert_with_id(
        &self,
        authority: &BootAuthority,
        id: MailboxId,
        name: String,
        entry: MailboxEntry,
    ) -> Result<MailboxId, NameConflict> {
        match self.apply_one(authority, RegistryEffect::publish_with_id(id, name, entry)) {
            Ok(RegistryApplied::Mailbox(id)) => Ok(id),
            Err(RegistryEffectError::Name(error)) => Err(error),
            Ok(_) | Err(_) => unreachable!("publish-live returns mailbox or name conflict"),
        }
    }

    /// Invalidate a live mailbox (ADR-0010). Transitions the entry
    /// to `Dropped` so dispatch-path readers can distinguish an
    /// intentional drop from an unknown id; the id itself (a function
    /// of the name per ADR-0029) stays addressable and a subsequent
    /// `try_register_inbox` / `register_inline` with the same
    /// name reuses it. Returns the released name on success.
    ///
    /// Issue 634 Phase 4 retired the dedicated `Component` variant,
    /// so this now drops any live `Inbox` or `Inline` mailbox.
    ///
    /// No production caller (iamacoffeepot/aether#4152 audited every one:
    /// all are tests). The `WasmTrampoline` shutdown path this comment
    /// used to name reaches [`CostTable::drop_mailbox`](crate::mail::cost::CostTable::drop_mailbox)
    /// now, which clears the per-handler cost cells and never touches a
    /// registry route.
    ///
    /// Direct write path — takes a [`BootAuthority`] like every other
    /// eager mutator (iamacoffeepot/aether#4161), so the remaining callers
    /// (all tests, which use it to clear a deliberately-installed collision
    /// route) name the same authority production boot would.
    ///
    /// # Panics
    /// Panics if the inner routing lock is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn drop_mailbox(&self, authority: &BootAuthority, id: MailboxId) -> Result<String, DropError> {
        match self.apply_one(authority, RegistryEffect::DropMailbox(id)) {
            Ok(RegistryApplied::Dropped(name)) => Ok(name),
            Err(RegistryEffectError::Drop(error)) => Err(error),
            Ok(_) | Err(_) => unreachable!("drop effect returns a name or drop error"),
        }
    }

    /// Register a mailbox whose handler body forwards the envelope
    /// into an actor's mpsc inbox. The actor's dispatch loop on its
    /// own thread runs the work and records the lifecycle
    /// `Received`/`Finished` bracket — `Mailer::push` does NOT
    /// bracket this arm. Use this for any registration where a
    /// dispatch loop downstream owns the per-handler invocation
    /// (chassis caps via `claim_mailbox*`, instanced + singleton
    /// actors via the spawner).
    ///
    /// **Wrong-variant symptom.** Picking [`Self::register_inbox`]
    /// for a synchronous handler — one that does immediate work on
    /// the pushing thread rather than enqueueing onto a downstream
    /// mpsc — leaks `in_flight` forever, because nothing downstream
    /// ever fires the `Finished` half of the bracket. Settlement
    /// subscribers on the parent chain hang. iamacoffeepot/aether#846
    /// is the canonical incident: `tick_fanout_propagates_chassis_root_lineage`
    /// used `register_inbox` for a `captured.push(...)` closure
    /// (synchronous Vec append, no downstream dispatcher), and once
    /// strict settlement propagation landed in
    /// `SubstrateHarness::run_frame` the test surfaced as a 5s
    /// `SettlementTimeout`. Fix: switch to [`Self::register_inline`].
    ///
    /// The dispatch-type asymmetry helps catch this — Inbox
    /// handlers receive [`OwnedDispatch`](crate::mail::registry::OwnedDispatch)
    /// so moving `payload` into a
    /// channel is natural; a synchronous body that doesn't move
    /// payload reads as "I should be Inline."
    ///
    /// # Panics
    /// Panics on a name collision (or if the inner routing lock is
    /// poisoned) — fail-fast per ADR-0063: substrate-internal
    /// registrations should never collide; use
    /// [`Self::try_register_inbox`] when a collision is a recoverable
    /// outcome rather than a bug.
    ///
    /// Direct write path — takes a [`BootAuthority`] so only the boot
    /// claim passes can name it (iamacoffeepot/aether#4161). A handler
    /// stages a `RegistryEffect` through the ADR-0165 owner instead.
    pub fn register_inbox(
        &self,
        authority: &BootAuthority,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> MailboxId {
        match self.insert(authority, name.into(), MailboxEntry::Inbox { handler, seize: SeizeCell::default() }) {
            Ok(id) => id,
            Err(NameConflict { name }) => {
                panic!("mailbox name already registered: {name}")
            }
        }
    }

    /// Non-panicking variant of [`Self::register_inbox`]. Returns
    /// `NameConflict` on a collision so callers that legitimately
    /// race (ADR-0070 capability boots, where the side-by-side
    /// extraction period puts legacy registrations and a new
    /// capability claim against the same mailbox during the
    /// transition diff) can surface the collision as a typed error
    /// rather than aborting the chassis.
    ///
    /// Direct write path — takes a [`BootAuthority`] so only the boot
    /// claim passes can name it (iamacoffeepot/aether#4161). A handler
    /// stages a `RegistryEffect` through the ADR-0165 owner instead.
    ///
    /// # Panics
    /// Panics if the inner routing lock is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn try_register_inbox(
        &self,
        authority: &BootAuthority,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> Result<MailboxId, NameConflict> {
        self.insert(authority, name.into(), MailboxEntry::Inbox { handler, seize: SeizeCell::default() })
    }

    /// ADR-0099 §3: [`Self::try_register_inbox`] but under an explicit,
    /// caller-computed `id` (the lineage fold) rather than `hash(name)`.
    /// The spawn path uses this for hosted / nested actors, whose id is
    /// the fold over their lineage; `name` stays the rendered display /
    /// reverse-map string. The returned id is `id` on success.
    ///
    /// Direct write path — takes a [`BootAuthority`] so only the boot /
    /// embedder eager spawn can name it (iamacoffeepot/aether#4156). A
    /// handler stages a `RegistryEffect` through the ADR-0165 owner
    /// instead.
    pub fn try_register_inbox_with_id(
        &self,
        authority: &BootAuthority,
        id: MailboxId,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> Result<MailboxId, NameConflict> {
        self.insert_with_id(authority, id, name.into(), MailboxEntry::Inbox { handler, seize: SeizeCell::default() })
    }

    /// Issue 838: register a mailbox whose handler runs inline on
    /// the pushing thread. `Mailer::push` brackets the call with
    /// `Received`/`Finished` so the chain's `in_flight` balances
    /// and settlement subscribers
    /// ([`crate::chassis::settlement::SettlementRegistry`]) wake
    /// (ADR-0080 §2).
    ///
    /// **Wrong-variant symptom.** Picking [`Self::register_inline`]
    /// for an actor-enqueue handler — one whose body forwards onto
    /// a downstream mpsc that another thread drains — double-counts
    /// `Finished`. The mailer fires the bracket around the enqueue,
    /// then the downstream dispatcher fires its own bracket when
    /// the envelope is picked up. Settlement subscribers wake on
    /// the first `Finished` — before the actual work runs — so
    /// callers proceed past the gate while the dispatcher is still
    /// processing the mail. Fix: switch to [`Self::register_inbox`].
    ///
    /// The dispatch-type asymmetry helps catch this — Inline
    /// handlers receive borrowed
    /// [`MailDispatch<'_>`](crate::mail::registry::MailDispatch) whose
    /// `payload: &[u8]` can't be moved into an mpsc without a
    /// `to_vec()` clone; that clone is the visible "I should be
    /// Inbox" smell.
    ///
    /// # Panics
    /// Panics on a name collision (or if the inner routing lock is
    /// poisoned) — fail-fast per ADR-0063: substrate-internal
    /// registrations should never collide.
    /// Direct write path — takes a [`BootAuthority`] so only the boot
    /// claim passes and the chassis diagnostic sinks can name it
    /// (iamacoffeepot/aether#4161). A handler stages a `RegistryEffect`
    /// through the ADR-0165 owner instead.
    pub fn register_inline(
        &self,
        authority: &BootAuthority,
        name: impl Into<String>,
        handler: Arc<dyn InlineHandler>,
    ) -> MailboxId {
        match self.insert(authority, name.into(), MailboxEntry::Inline(handler)) {
            Ok(id) => id,
            Err(NameConflict { name }) => {
                panic!("mailbox name already registered: {name}")
            }
        }
    }

    /// Issue 607 Phase 7: fully remove a registered mailbox. Used in
    /// the chassis-boot unwind path when a singleton's `init` fails
    /// after `try_register_inbox` claimed the slot — the partial-
    /// boot state must not leak into a later cap's namespace lookup.
    /// Returns `true` if the entry existed and was a live (`Inbox`
    /// or `Inline`) variant and was removed; `false` if the id is
    /// unknown or already in `Dropped` state. Component entries go
    /// through [`Self::drop_mailbox`] (which transitions to
    /// `Dropped` rather than removing) — the lifecycle difference
    /// is intentional: components can re-register the same id after
    /// a drop, chassis-bound mailboxes are torn down on cap
    /// teardown and the id can be freshly recreated.
    ///
    /// Direct write path — takes a [`BootAuthority`] so only the boot
    /// unwind and the boot / embedder eager spawn can name it
    /// (iamacoffeepot/aether#4156).
    pub(crate) fn remove_closure(&self, authority: &BootAuthority, id: MailboxId) -> bool {
        match self.apply_one(authority, RegistryEffect::RemoveMailbox(id)) {
            Ok(RegistryApplied::Removed(removed)) => removed,
            Ok(_) | Err(_) => unreachable!("remove effect is infallible and returns a bool"),
        }
    }

    /// Install a `Pooled` actor's [`SeizeHandle`]
    /// onto its `Inbox` entry's deferred [`SeizeCell`] so the blob
    /// demuxer can resolve recipient → slot and dispatch in place
    /// (ADR-0087 §4, iamacoffeepot/aether#1135). Called by the
    /// `Pooled`-branch wiring in `chassis/builder.rs` +
    /// `actor/native/spawn/builder.rs` once the dispatcher slot exists. Returns
    /// `true` on a successful install; `false` if the id isn't a live
    /// `Inbox` entry or the cell was already populated (idempotent — one
    /// install per slot in production).
    ///
    /// Direct write path — takes a [`BootAuthority`] so only the boot /
    /// embedder eager spawn can name it (iamacoffeepot/aether#4156).
    ///
    /// # Panics
    /// Panics if the inner routing lock is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn install_seize_handle(&self, authority: &BootAuthority, id: MailboxId, handle: SeizeHandle) -> bool {
        match self.apply_one(authority, RegistryEffect::InstallSeize { id, handle }) {
            Ok(RegistryApplied::SeizeInstalled(installed)) => installed,
            Ok(_) | Err(_) => unreachable!("seize effect is infallible and returns a bool"),
        }
    }
}
