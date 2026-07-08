//! The guest authoring surface (ADR-0137), behind the `runtime` feature.
//!
//! This is what a behavior script's handlers touch: [`BehaviorCtx`] and its
//! [`WidgetHandle`] / [`ChildHandle`] / [`PanelHandle`] trinity, the
//! decode-once [`last`](WidgetHandle::last) mirror, the effect accumulator,
//! and the [`Behavior`] lifecycle trait. The module compiles FFI-free on
//! native so host-side tests drive the ctx / mirror / drain logic directly;
//! the guest FFI helpers (`leak_packed`, `read_guest_slice`) are the only
//! `wasm`-gated surface, and the `#[behavior]` macro's four exports are the
//! only callers.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::any::Any;
use core::cell::UnsafeCell;

use aether_data::wire;
use aether_data::{Kind, KindId};
use serde::de::DeserializeOwned;

use crate::envelope::{Effect, EffectTarget, FilterOutput, Verdict, encode};
use crate::sentinel;

/// One entry in a handle's mirror: the raw kind bytes plus a lazily-filled
/// decode cache. Storing bytes and decoding on read is the only shape that
/// works generically over an open `K` — a per-kind typed slot is
/// impossible without naming every custom-widget kind ahead of time.
struct MirrorSlot {
    bytes: Vec<u8>,
    cache: Option<Box<dyn Any>>,
}

impl MirrorSlot {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, cache: None }
    }

    /// Replace the stored bytes and drop any cached decode — the next
    /// `last::<K>()` re-decodes from the new bytes.
    fn set(&mut self, bytes: Vec<u8>) {
        self.bytes = bytes;
        self.cache = None;
    }
}

/// A per-handle last-value-per-kind mirror. Writes pay serialize-once-per-
/// *change* (bytes already in flight); reads pay decode-once-per-*change*.
#[derive(Default)]
struct Mirror {
    slots: BTreeMap<KindId, MirrorSlot>,
}

impl Mirror {
    /// Record a kind's latest bytes, invalidating the decode cache.
    fn update(&mut self, id: KindId, bytes: Vec<u8>) {
        match self.slots.get_mut(&id) {
            Some(slot) => slot.set(bytes),
            None => {
                self.slots.insert(id, MirrorSlot::new(bytes));
            }
        }
    }

    /// The last value seen for `K`, decoded once and cached. Returns `None`
    /// if no `K` has flowed through this handle, or if the stored bytes no
    /// longer decode as `K`.
    fn last<K: Kind + 'static>(&mut self) -> Option<&K> {
        let slot = self.slots.get_mut(&K::ID)?;
        if slot.cache.is_none() {
            let decoded = K::decode_from_bytes(&slot.bytes)?;
            slot.cache = Some(Box::new(decoded));
        }
        slot.cache.as_deref()?.downcast_ref::<K>()
    }
}

/// The in-flight mail verdict as the ctx accumulates it, projected to the
/// wire [`Verdict`] at drain. Kept internal so author code never names a
/// verdict type — mutation *is* the verdict.
enum VerdictState {
    /// The default: forward the inbound bytes unchanged (a `&K` observer,
    /// or a lifecycle call with no inbound mail).
    ForwardOriginal,
    /// A `&mut K` handler mutated the kind; forward the re-encoded bytes.
    ForwardMutated(Vec<u8>),
    /// `ctx.consume()` dropped the mail.
    Consume,
}

/// Persistent last-value mirrors for one behavior-script instance.
#[derive(Default)]
pub struct MirrorStore {
    widget_mirror: Mirror,
    panel_mirror: Mirror,
    child_mirrors: BTreeMap<String, Mirror>,
}

/// The pure filter-call context handed to every handler. Accumulates
/// effects and the verdict, and borrows the decode-once mirror store each
/// handle reads. The host builds one per `filter` call and drains it after.
pub struct BehaviorCtx<'m> {
    inbound: Vec<u8>,
    verdict: VerdictState,
    effects: Vec<Effect>,
    mirrors: &'m mut MirrorStore,
}

impl<'m> BehaviorCtx<'m> {
    /// Build a ctx for an inbound `filter` call, seeding the widget mirror
    /// from the inbound kind (the kind flowing through the interposition
    /// updates the mirror before handler dispatch, ADR-0137). A sentinel
    /// kind carries no payload, so it seeds nothing.
    #[doc(hidden)]
    #[must_use]
    pub fn __new_inbound(mirrors: &'m mut MirrorStore, kind_id: KindId, bytes: &[u8]) -> Self {
        let ctx = Self {
            inbound: bytes.to_vec(),
            verdict: VerdictState::ForwardOriginal,
            effects: Vec::new(),
            mirrors,
        };
        if !sentinel::is_sentinel(kind_id) {
            ctx.mirrors.widget_mirror.update(kind_id, bytes.to_vec());
        }
        ctx
    }

    /// The wrapped widget the host interposes on.
    #[must_use]
    pub fn widget(&mut self) -> WidgetHandle<'_, 'm> {
        WidgetHandle { ctx: self }
    }

    /// The parent lane.
    #[must_use]
    pub fn panel(&mut self) -> PanelHandle<'_, 'm> {
        PanelHandle { ctx: self }
    }

    /// A named child in the host's subtree, addressed path-relative.
    #[must_use]
    pub fn child(&mut self, path: &str) -> ChildHandle<'_, 'm> {
        ChildHandle {
            ctx: self,
            path: String::from(path),
        }
    }

    /// Drop the in-flight mail. Effects still apply — consume plus an
    /// emitted effect substitutes for a forward.
    pub fn consume(&mut self) {
        self.verdict = VerdictState::Consume;
    }

    /// Record a `&mut K` handler's re-encoded bytes as the forward payload.
    /// A prior `consume()` wins — the drop is not undone by the unconditional
    /// re-encode the dispatch table emits after every intercept handler.
    #[doc(hidden)]
    pub fn __forward_mutated(&mut self, bytes: Vec<u8>) {
        if !matches!(self.verdict, VerdictState::Consume) {
            self.verdict = VerdictState::ForwardMutated(bytes);
        }
    }

    /// Consume the ctx into the wire [`FilterOutput`] the host drains.
    #[doc(hidden)]
    #[must_use]
    pub fn __into_output(self) -> FilterOutput {
        let verdict = match self.verdict {
            VerdictState::ForwardOriginal => Verdict::Forward(self.inbound),
            VerdictState::ForwardMutated(bytes) => Verdict::Forward(bytes),
            VerdictState::Consume => Verdict::Consume,
        };
        FilterOutput {
            verdict,
            effects: self.effects,
        }
    }
}

/// Handle to the wrapped widget: intercept-time writes (`set`), the mirror
/// read (`last`), and a replay request (`report`).
pub struct WidgetHandle<'c, 'm> {
    ctx: &'c mut BehaviorCtx<'m>,
}

impl WidgetHandle<'_, '_> {
    /// Write a kind to the widget. The effect is drained into a real send
    /// after the filter returns; the write also integrates into the mirror
    /// so a later `last::<K>()` reflects the script's own write (echo
    /// suppression, truthful mirror).
    pub fn set<K: Kind + 'static>(&mut self, value: &K) {
        let bytes = value.encode_into_bytes();
        self.ctx.mirrors.widget_mirror.update(K::ID, bytes.clone());
        self.ctx.effects.push(Effect {
            target: EffectTarget::Widget,
            kind_id: K::ID.0,
            bytes,
        });
    }

    /// The last value the widget emitted for `K`, decoded once.
    pub fn last<K: Kind + 'static>(&mut self) -> Option<&K> {
        self.ctx.mirrors.widget_mirror.last::<K>()
    }

    /// Ask the widget to re-emit its observable kinds up-lane. The reply is
    /// that traffic filling the mirror, not a return value.
    pub fn report(&mut self) {
        self.ctx.effects.push(Effect {
            target: EffectTarget::Widget,
            kind_id: sentinel::REPORT.0,
            bytes: Vec::new(),
        });
    }
}

/// Handle to a named child: a send (`send`), the mirror read (`last`), and a
/// replay request (`report`).
pub struct ChildHandle<'c, 'm> {
    ctx: &'c mut BehaviorCtx<'m>,
    path: String,
}

impl ChildHandle<'_, '_> {
    /// Send a kind to the child. Drained into a real cluster send after the
    /// filter returns; the write integrates into the child's mirror.
    pub fn send<K: Kind + 'static>(&mut self, value: &K) {
        let bytes = value.encode_into_bytes();
        self.ctx
            .mirrors
            .child_mirrors
            .entry(self.path.clone())
            .or_default()
            .update(K::ID, bytes.clone());
        self.ctx.effects.push(Effect {
            target: EffectTarget::Child(self.path.clone()),
            kind_id: K::ID.0,
            bytes,
        });
    }

    /// The last value the child emitted for `K`, decoded once.
    pub fn last<K: Kind + 'static>(&mut self) -> Option<&K> {
        self.ctx
            .mirrors
            .child_mirrors
            .get_mut(&self.path)?
            .last::<K>()
    }

    /// Ask the child to re-emit its observable kinds up-lane.
    pub fn report(&mut self) {
        self.ctx.effects.push(Effect {
            target: EffectTarget::Child(self.path.clone()),
            kind_id: sentinel::REPORT.0,
            bytes: Vec::new(),
        });
    }
}

/// Handle to the parent lane: emit up (`emit`) and the mirror read (`last`).
pub struct PanelHandle<'c, 'm> {
    ctx: &'c mut BehaviorCtx<'m>,
}

impl PanelHandle<'_, '_> {
    /// Emit a kind up the parent lane. Drained into a real send after the
    /// filter returns; the write integrates into the panel mirror.
    pub fn emit<K: Kind + 'static>(&mut self, value: &K) {
        let bytes = value.encode_into_bytes();
        self.ctx.mirrors.panel_mirror.update(K::ID, bytes.clone());
        self.ctx.effects.push(Effect {
            target: EffectTarget::Panel,
            kind_id: K::ID.0,
            bytes,
        });
    }

    /// The last value seen on the parent lane for `K`, decoded once.
    pub fn last<K: Kind + 'static>(&mut self) -> Option<&K> {
        self.ctx.mirrors.panel_mirror.last::<K>()
    }
}

/// The lifecycle contract a `#[behavior]` script fulfills (ADR-0137). The
/// three lifecycle hooks default to no-ops and are dispatched from `filter`
/// on the reserved [`sentinel`]s; `state_save` / `state_load` are the
/// migration blob accessors the `state_save` / `state_load` guest exports
/// call, and the `#[behavior]` macro emits their default serde bodies (over
/// `state_save_serde` / `state_load_serde`) unless the author overrides them.
pub trait Behavior: Sized {
    /// Runs post-restore with mirrors primed and ctx available.
    fn on_attach(&mut self, ctx: &mut BehaviorCtx<'_>) {
        let _ = ctx;
    }

    /// Per-frame work, dispatched on the SDK-owned frame sentinel (no kit
    /// coupling).
    fn on_frame(&mut self, ctx: &mut BehaviorCtx<'_>) {
        let _ = ctx;
    }

    /// Best-effort teardown as the script leaves its position.
    fn on_detach(&mut self, ctx: &mut BehaviorCtx<'_>) {
        let _ = ctx;
    }

    /// Serialize the migration state blob.
    fn state_save(&self) -> Vec<u8>;

    /// Restore from a migration state blob.
    fn state_load(&mut self, bytes: &[u8]);
}

/// Default `state_save` body the `#[behavior]` macro emits — serialize the
/// author's struct through the `no_std` wire codec. The `Serialize` bound
/// lands at the macro's emitted call site, so a behavior using the default
/// must derive `Serialize` (author-derived, as `#[actor]` components derive
/// on their kind types).
#[must_use]
pub fn state_save_serde<T: serde::Serialize>(value: &T) -> Vec<u8> {
    wire::to_vec(value).unwrap_or_default()
}

/// Default `state_load` body the `#[behavior]` macro emits — overwrite the
/// author's struct from a prior wire blob, leaving it untouched on an
/// undecodable blob (fail-open).
pub fn state_load_serde<T: DeserializeOwned>(slot: &mut T, bytes: &[u8]) {
    if let Ok(value) = wire::from_bytes(bytes) {
        *slot = value;
    }
}

/// Build a `FilterOutput`'s encoded bytes for one filter call: seed the ctx
/// from the inbound `(kind_id, bytes)`, run the macro-generated `dispatch`,
/// drain, and encode. The guest `filter` shim wraps the result with
/// `leak_packed`; host tests drive [`BehaviorCtx`] directly.
#[must_use]
pub fn run_filter(
    mirrors: &mut MirrorStore,
    kind_id: KindId,
    inbound: &[u8],
    dispatch: impl FnOnce(&mut BehaviorCtx<'_>),
) -> Vec<u8> {
    let mut ctx = BehaviorCtx::__new_inbound(mirrors, kind_id, inbound);
    dispatch(&mut ctx);
    encode(&ctx.__into_output())
}

/// Single-instance backing store for the macro-emitted `static` script
/// slot. A wasm guest is single-threaded (ADR-0010 §5) and the host
/// serializes `filter` / `state_*` calls, so an `UnsafeCell` with a blanket
/// `Sync` impl is sound — the same argument that licenses `aether-actor`'s
/// component slot. The behavior struct is instantiated lazily via `Default`
/// on first access, since the four-export ABI carries no constructor.
pub struct Slot<T> {
    cell: UnsafeCell<Option<T>>,
}

impl<T> Slot<T> {
    /// Build an empty slot. `const` so it can live in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cell: UnsafeCell::new(None),
        }
    }
}

impl<T: Default> Slot<T> {
    /// Borrow the instance, constructing it via `Default` on first access.
    // Returning `&mut T` from `&self` is the load-bearing interior-mutability
    // pattern here; the `UnsafeCell` makes it sound under the host's
    // serialized-dispatch guarantee, the same exception `aether-actor`'s
    // slot carries.
    #[allow(clippy::mut_from_ref)]
    pub fn get_or_default(&self) -> &mut T {
        // SAFETY: single-threaded guest + host-serialized entry points mean
        // no other live reference to the cell exists at this call.
        let opt = unsafe { &mut *self.cell.get() };
        opt.get_or_insert_with(T::default)
    }
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: single-threaded wasm guest + host-serialized FFI entry points mean
// the `UnsafeCell` is only ever touched from one thread at a time — no
// concurrent access is possible inside one wasm linear memory.
unsafe impl<T> Sync for Slot<T> {}

/// `cabi_realloc`-shaped guest allocator backing the `alloc` export: the
/// host obtains a region from it, writes the inbound payload, then calls
/// `filter` / `state_load`. Not target-gated (plain `alloc`-crate code, so
/// it is host-testable); the `alloc` FFI shim that calls it is `wasm`-only.
///
/// # Safety
/// `old_ptr` / `old_size` / `align` must describe a live allocation from a
/// prior call (or `(0, 0)` for a fresh allocation), and `align` must be a
/// nonzero power of two.
pub unsafe fn realloc_bytes(
    old_ptr: *mut u8,
    old_size: usize,
    align: usize,
    new_size: usize,
) -> *mut u8 {
    use alloc::alloc::{Layout, alloc, dealloc, realloc};
    use core::ptr::null_mut;

    if new_size == 0 {
        if !old_ptr.is_null() {
            // SAFETY: caller's layout contract holds; `old_ptr` is a live
            // allocation described by `(old_size, align)`.
            unsafe {
                dealloc(old_ptr, Layout::from_size_align_unchecked(old_size, align));
            }
        }
        return null_mut();
    }
    if old_ptr.is_null() {
        // SAFETY: caller's layout contract holds; a fresh allocation.
        return unsafe { alloc(Layout::from_size_align_unchecked(new_size, align)) };
    }
    // SAFETY: caller's layout contract holds; `old_ptr` is a live allocation
    // described by `(old_size, align)`, resized to `new_size`.
    unsafe {
        realloc(
            old_ptr,
            Layout::from_size_align_unchecked(old_size, align),
            new_size,
        )
    }
}

/// Leak an owned byte buffer into guest memory and pack its `(ptr, len)` for
/// return across the FFI. The host reads the region and never frees it
/// (wasm has no `memory.shrink`), so the leak is the contract, not a bug.
#[cfg(target_family = "wasm")]
#[must_use]
pub fn leak_packed(bytes: Vec<u8>) -> u64 {
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len() as u32;
    let ptr = boxed.as_ptr() as u32;
    core::mem::forget(boxed);
    crate::abi::pack_ptr_len(ptr, len)
}

/// Borrow a host-written `(ptr, len)` region as a byte slice. A zero length
/// short-circuits to an empty slice so a null pointer is never dereferenced.
///
/// # Safety
/// The host wrote `len` bytes at `ptr` (via the `alloc` export); the slice
/// is bounded by the current call, which finishes before the host reuses the
/// region.
#[cfg(target_family = "wasm")]
#[must_use]
pub unsafe fn read_guest_slice<'a>(ptr: u32, len: u32) -> &'a [u8] {
    if len == 0 {
        &[]
    } else {
        // SAFETY: caller upholds the region contract above.
        unsafe { core::slice::from_raw_parts(ptr as usize as *const u8, len as usize) }
    }
}

#[cfg(test)]
mod tests;
