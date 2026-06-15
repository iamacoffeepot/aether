//! Inline-child registry + receive membrane (ADR-0114 decisions #2/#3).
//!
//! An inline child shares its parent's WASM instance, slot, and
//! run-token (ADR-0114). [`FfiCtx::spawn_inline_child`] inserts
//! the constructed child into the process-global [`INLINE_CHILDREN`]
//! registry keyed by the child's alias [`MailboxId`]; the
//! [`crate::export!`] `receive_p32` shims route every inbound mail
//! through [`membrane_dispatch`], which dispatches the parent when the
//! routed recipient is the parent's own id and otherwise demuxes to the
//! co-located child the producer addressed.
//!
//! The registry is slot-shaped (take-out / dispatch / reinsert) so a
//! running child can spawn or mutate siblings through `ctx` while it is
//! itself dispatched — the registry borrow is never held across a child's
//! `erased_dispatch`. The guest is single-threaded (ADR-0010 §5) and the
//! substrate serializes delivery under the run token, so an `UnsafeCell`
//! with a blanket `Sync` impl is sound — the same argument that licenses
//! [`crate::Slot`].

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;

use aether_data::MailboxId;

use crate::ffi::ErasedFfiActor;
use crate::ffi::ctx::FfiCtx;
use crate::mail::Mail;

/// One inline child's slot. `actor` is `None` while the child is taken
/// out for dispatch (the slot-shaped take / reinsert) and `Some` at rest.
///
/// ADR-0114 §5: the slot also records the metadata a `replace_component`
/// swap needs to reconstruct the child in the fresh instance — the
/// actor-type tag (`mailbox_id_from_name(NAMESPACE)`, the same tag
/// `init_typed_p32` matches a reconstruct on) plus the resolved
/// `full_subname` / `is_counter` the alias id was folded from, so the
/// rehydrate path re-folds the identical alias and re-`init`s the child
/// by type.
struct InlineSlot {
    id: MailboxId,
    /// `mailbox_id_from_name(A::NAMESPACE)` — the actor-type tag the
    /// rehydrate reconstruct matches against the module's exported types.
    type_tag: u64,
    /// The resolved discriminator the alias id was folded from (a counter
    /// child's monotonic value is already resolved here, not the
    /// unresolved `Counter` marker), so re-folding on rehydrate is
    /// deterministic.
    full_subname: String,
    /// Whether the host should treat `full_subname` as a counter prefix on
    /// re-fold; always `false` after resolution, but carried so the
    /// rehydrate call mirrors the original `spawn_inline_child` shape.
    is_counter: bool,
    actor: Option<Box<dyn ErasedFfiActor>>,
}

/// A cloneable snapshot of one resident inline child's reconstruct
/// metadata (no actor box), produced by [`InlineRegistry::child_metas`]
/// for the dehydrate walk. The compose path reads each child's state
/// through [`InlineRegistry::with_child_mut`] keyed by `id`.
#[derive(Clone)]
pub struct InlineChildMeta {
    /// The child's alias [`MailboxId`] (the registry key).
    pub id: MailboxId,
    /// The actor-type tag — `mailbox_id_from_name(NAMESPACE)`.
    pub type_tag: u64,
    /// The resolved subname the alias id was folded from.
    pub full_subname: String,
    /// Whether the original spawn used a counter discriminator.
    pub is_counter: bool,
}

/// The process-global inline-child registry (ADR-0114 decision #3), keyed
/// by each child's alias [`MailboxId`]. The membrane demuxes the inbound
/// recipient against it.
pub struct InlineRegistry {
    inner: UnsafeCell<Vec<InlineSlot>>,
}

// SAFETY: identical argument to [`crate::Slot`] — the WASM guest is
// single-threaded (ADR-0010 §5) and the substrate serializes delivery
// under the run token, so `INLINE_CHILDREN` is only ever touched from one
// thread at a time. On the host unit-test build the static is reached
// from one test thread.
unsafe impl Sync for InlineRegistry {}

impl InlineRegistry {
    /// An empty registry. `const` so it can back a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(Vec::new()),
        }
    }

    /// Register a freshly-spawned (or reconstructed) inline child under
    /// `id`, recording the reconstruct metadata alongside the actor box.
    /// Replaces the actor + metadata if `id` is already present (a
    /// re-spawn / rehydrate re-register of the same alias).
    pub fn insert_child(
        &self,
        id: MailboxId,
        type_tag: u64,
        full_subname: String,
        is_counter: bool,
        actor: Box<dyn ErasedFfiActor>,
    ) {
        // SAFETY: single-threaded guest + serialized delivery — no other
        // live borrow of the cell (the `Sync` argument). The borrow is
        // released before this returns, so it never spans a dispatch.
        let slots = unsafe { &mut *self.inner.get() };
        if let Some(slot) = slots.iter_mut().find(|s| s.id == id) {
            slot.type_tag = type_tag;
            slot.full_subname = full_subname;
            slot.is_counter = is_counter;
            slot.actor = Some(actor);
        } else {
            slots.push(InlineSlot {
                id,
                type_tag,
                full_subname,
                is_counter,
                actor: Some(actor),
            });
        }
    }

    /// Take the child out for dispatch, leaving its slot (and its
    /// reconstruct metadata) intact but the actor box empty. Returns
    /// `None` if `id` names no resident inline child (already taken out,
    /// or never registered). The borrow drops before the returned box is
    /// dispatched, so a child may re-enter the registry mid-dispatch.
    pub fn take(&self, id: MailboxId) -> Option<Box<dyn ErasedFfiActor>> {
        // SAFETY: see [`Self::insert_child`].
        let slots = unsafe { &mut *self.inner.get() };
        slots
            .iter_mut()
            .find(|s| s.id == id)
            .and_then(|s| s.actor.take())
    }

    /// Put a child back after dispatch, into its existing slot (metadata
    /// preserved). Pairs with [`Self::take`]; the slot is guaranteed to
    /// exist because `take` left it in place with an empty actor box.
    pub fn reinsert(&self, id: MailboxId, actor: Box<dyn ErasedFfiActor>) {
        // SAFETY: see [`Self::insert_child`].
        let slots = unsafe { &mut *self.inner.get() };
        if let Some(slot) = slots.iter_mut().find(|s| s.id == id) {
            slot.actor = Some(actor);
        }
    }

    /// Snapshot the reconstruct metadata of every resident inline child
    /// (ADR-0114 §5 dehydrate walk). The actor boxes stay in the
    /// registry; the compose path reads each child's state through
    /// [`Self::with_child_mut`] keyed by the returned `id`.
    #[must_use]
    pub fn child_metas(&self) -> Vec<InlineChildMeta> {
        // SAFETY: see [`Self::insert_child`].
        let slots = unsafe { &*self.inner.get() };
        slots
            .iter()
            .map(|slot| InlineChildMeta {
                id: slot.id,
                type_tag: slot.type_tag,
                full_subname: slot.full_subname.clone(),
                is_counter: slot.is_counter,
            })
            .collect()
    }

    /// Run `f` against the child registered under `id` with a unique
    /// mutable borrow held only for the call, returning its result (or
    /// `None` if `id` names no resident child). Used by the dehydrate
    /// compose to drive each child's `erased_on_dehydrate` in place. The
    /// borrow drops before this returns, so it never spans a dispatch.
    pub fn with_child_mut<R>(
        &self,
        id: MailboxId,
        f: impl FnOnce(&mut dyn ErasedFfiActor) -> R,
    ) -> Option<R> {
        // SAFETY: see [`Self::insert_child`].
        let slots = unsafe { &mut *self.inner.get() };
        slots
            .iter_mut()
            .find(|s| s.id == id)
            .and_then(|s| s.actor.as_deref_mut())
            .map(f)
    }
}

impl Default for InlineRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The registry the [`crate::export!`] membrane and
/// [`FfiCtx::spawn_inline_child`] share.
pub static INLINE_CHILDREN: InlineRegistry = InlineRegistry::new();

/// ADR-0114 decision #3: the receive membrane every `export!`
/// `receive_p32` shim routes inbound mail through. When the routed
/// recipient is the parent's own mailbox id, dispatch the parent
/// (`dispatch_own`); otherwise take the inline child the producer
/// addressed out of [`INLINE_CHILDREN`], dispatch it with a ctx
/// self-identified as the child ([`FfiCtx::__new`]), and reinsert. An
/// unrecognised recipient falls back to the parent's dispatch — the
/// existing unmatched path (the parent's `#[fallback]`, or the
/// `DISPATCH_UNKNOWN_KIND` sentinel for a strict receiver), never a
/// short-circuit drop.
///
/// For a normal (non-inline) actor the routed recipient equals the
/// parent's own id, so the membrane no-ops straight to `dispatch_own` —
/// the regression guard the whole demux rests on.
pub fn membrane_dispatch<F>(own_mailbox_id: u64, mail: Mail<'_>, dispatch_own: F) -> u32
where
    F: FnOnce(Mail<'_>) -> u32,
{
    let recipient = mail.recipient().0;
    if recipient == own_mailbox_id {
        return dispatch_own(mail);
    }
    let id = MailboxId(recipient);
    match INLINE_CHILDREN.take(id) {
        Some(mut child) => {
            let mut ctx = FfiCtx::__new(recipient);
            let rc = child.erased_dispatch(&mut ctx, mail);
            INLINE_CHILDREN.reinsert(id, child);
            rc
        }
        // An alias whose child isn't resident (a race against teardown, or
        // a stray address) runs the parent's unmatched path rather than
        // dropping the mail silently.
        None => dispatch_own(mail),
    }
}

#[cfg(test)]
mod tests {
    use super::{INLINE_CHILDREN, InlineRegistry, membrane_dispatch};
    use crate::FfiCtx;
    use crate::ffi::ErasedFfiActor;
    use crate::mail::{Mail, PriorState};
    use aether_data::MailboxId;
    use alloc::boxed::Box;
    use alloc::string::String;
    use core::sync::atomic::{AtomicU32, Ordering};

    /// Distinct return codes so an assertion can tell which dispatch path
    /// the membrane took.
    const OWN_CODE: u32 = 0xA0;
    const CHILD_CODE: u32 = 0xC0;

    /// Process-global dispatch counter the recording child bumps, so a
    /// test can prove a child taken out for dispatch was reinserted (a
    /// second dispatch lands again).
    static CHILD_DISPATCHES: AtomicU32 = AtomicU32::new(0);

    /// Minimal `ErasedFfiActor` for the membrane tests: records each
    /// dispatch and returns [`CHILD_CODE`]. The lifecycle hooks are
    /// unreachable in these tests.
    struct RecordingChild;

    impl ErasedFfiActor for RecordingChild {
        fn erased_namespace(&self) -> &'static str {
            "test.inline.recording_child"
        }
        fn erased_dispatch(
            &mut self,
            _ctx: &mut FfiCtx<'_, crate::Manual>,
            _mail: Mail<'_>,
        ) -> u32 {
            CHILD_DISPATCHES.fetch_add(1, Ordering::Relaxed);
            CHILD_CODE
        }
        fn erased_wire(&mut self, _ctx: &mut FfiCtx<'_, crate::Manual>) {}
        fn erased_unwire(&mut self, _ctx: &mut FfiCtx<'_, crate::Manual>) {}
        fn erased_on_dehydrate(&mut self, _ctx: &mut crate::FfiDropCtx<'_>) {}
        fn erased_on_rehydrate(
            &mut self,
            _ctx: &mut FfiCtx<'_, crate::Manual>,
            _prior: PriorState<'_>,
        ) {
        }
    }

    /// Build a host-side `Mail` with the given routed recipient; the
    /// payload pointer is never dereferenced by these tests (the
    /// recording child doesn't decode), so a dangling-but-unread `ptr`
    /// with `byte_len = 0` is fine.
    fn mail_to(recipient: u64) -> Mail<'static> {
        // SAFETY: `byte_len = 0` so no bytes at `ptr` are ever read; the
        // membrane and `RecordingChild` only inspect `recipient`.
        unsafe { Mail::__from_ptr(0, 1, 0, 1, crate::NO_REPLY_HANDLE, recipient) }
    }

    /// Step 3 coverage: the slot-shaped registry round-trips a child
    /// through insert → take → reinsert → take.
    #[test]
    fn registry_insert_take_reinsert_round_trips() {
        let registry = InlineRegistry::new();
        let id = MailboxId(0x1111);

        assert!(registry.take(id).is_none(), "empty registry has no child");
        registry.insert_child(
            id,
            0,
            String::from("widget"),
            false,
            Box::new(RecordingChild),
        );
        let taken = registry
            .take(id)
            .expect("insert then take returns the child");
        assert!(
            registry.take(id).is_none(),
            "a taken-out slot is empty until reinsert",
        );
        registry.reinsert(id, taken);
        assert!(
            registry.take(id).is_some(),
            "reinsert refills the slot for the next dispatch",
        );
    }

    /// Step 1 coverage: a spawned child's slot carries its actor-type tag
    /// and resolved subname, surfaced through `child_metas` for the
    /// dehydrate walk.
    #[test]
    fn child_metas_carry_type_tag_and_subname() {
        let registry = InlineRegistry::new();
        let id = MailboxId(0x7777);
        let tag = 0xABCD_u64;
        registry.insert_child(
            id,
            tag,
            String::from("widget"),
            false,
            Box::new(RecordingChild),
        );

        let metas = registry.child_metas();
        let meta = match metas.as_slice() {
            [one] => one,
            other => panic!("expected exactly one child meta, got {}", other.len()),
        };
        assert_eq!(meta.id, id, "the meta carries the alias id");
        assert_eq!(meta.type_tag, tag, "the meta carries the actor-type tag");
        assert_eq!(meta.full_subname, "widget", "the meta carries the subname");
        assert!(!meta.is_counter, "a Named subname is not a counter");
    }

    /// Step 4 coverage: recipient == own id dispatches the parent, never
    /// the child registry.
    #[test]
    fn membrane_routes_own_recipient_to_parent() {
        let own = 0x2000_u64;
        let rc = membrane_dispatch(own, mail_to(own), |_mail| OWN_CODE);
        assert_eq!(rc, OWN_CODE, "own-id recipient runs the parent dispatch");
    }

    /// Step 4 coverage: a child-addressed recipient dispatches the child
    /// and reinserts it, so a second send to the same alias dispatches
    /// again (the take/reinsert round-trip under the membrane).
    #[test]
    fn membrane_routes_child_recipient_and_reinserts() {
        let own = 0x3000_u64;
        let child = 0x3001_u64;
        let before = CHILD_DISPATCHES.load(Ordering::Relaxed);
        INLINE_CHILDREN.insert_child(
            MailboxId(child),
            0,
            String::from("widget"),
            false,
            Box::new(RecordingChild),
        );

        let rc = membrane_dispatch(own, mail_to(child), |_mail| {
            panic!("own dispatch must not run for a child recipient")
        });
        assert_eq!(rc, CHILD_CODE, "child recipient runs the child dispatch");

        // Reinserted: a second send to the same alias dispatches again.
        let rc2 = membrane_dispatch(own, mail_to(child), |_mail| {
            panic!("own dispatch must not run for a reinserted child")
        });
        assert_eq!(rc2, CHILD_CODE, "the child was reinserted after dispatch");
        assert_eq!(
            CHILD_DISPATCHES.load(Ordering::Relaxed) - before,
            2,
            "both sends reached the child",
        );
    }

    /// Step 4 coverage: an unrecognised recipient (no resident child) runs
    /// the parent's unmatched path rather than short-circuit dropping.
    #[test]
    fn membrane_routes_unknown_recipient_to_parent_unmatched_path() {
        let own = 0x4000_u64;
        let stray = 0x4999_u64;
        let rc = membrane_dispatch(own, mail_to(stray), |_mail| OWN_CODE);
        assert_eq!(
            rc, OWN_CODE,
            "an unknown recipient falls back to the parent's unmatched path",
        );
    }
}
