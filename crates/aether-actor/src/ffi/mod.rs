//! FFI-actor binding layer. The contract: any host that exposes the
//! `_p32`-suffixed import surface (today: the wasm runtime in
//! `aether-substrate::actor::wasm`; future: a C host, an OS-process
//! host, ...) can drive an actor through this module.
//!
//! Surface:
//!
//!   - [`raw`] — `extern "C"` host-fn imports + host-target panic
//!     stubs (the only place the `_p32` symbols are named).
//!   - [`bridge`] — per-concern free-function modules (`bridge::mail`,
//!     `bridge::persist`). Each module owns one FFI op family and
//!     forwards calls to the matching `raw::*` host fn. Issue 665 split
//!     the prior monolithic `MailTransport`-impl ZST into these per-concern
//!     modules so persistence isn't mixed with mail; issue 1967 collapsed
//!     the per-module ZST + static packaging into free functions.
//!   - [`FfiInitCtx`] / [`FfiCtx`] / [`FfiDropCtx`] — concrete per-stage
//!     ctx structs, each impling the relevant subset of the per-stage
//!     capability traits in [`crate::actor::ctx`].
//!   - [`FfiActorMailbox<R>`] — actor-typed sender returned by
//!     `ctx.actor::<R>()` / `ctx.resolve_actor::<R>(name)`. Lifetime-
//!     free — the bridge free functions cover dispatch.
//!   - [`FfiActor`] trait — entry point with the `init` constructor and
//!     the `wire` / `unwire` / `on_dehydrate` / `on_rehydrate` lifecycle
//!     hooks (ADR-0101). `init` returns `Result<Self, BootError>` so a
//!     guest can surface its own error message instead of the panic-hook
//!     path's generic "guest trapped during init" text.
//!   - [`crate::export!`] — `#[no_mangle]` `init` / `receive` /
//!     lifecycle shims plus the `aether.kinds.inputs` /
//!     `aether.namespace` custom-section pins.
//!
//! Issue 663 renamed this module from `wasm` to `ffi`. The substrate
//! side keeps the wasm naming (`aether_substrate::actor::wasm`)
//! because that *is* the wasm runtime; the FFI binding layer here is
//! generic. Wire-level FFI ABI names (`init`, `receive_p32`,
//! `_p32` suffix, `aether.kinds.inputs` / `aether.namespace`
//! link-section names) stay unchanged — they are the on-the-wire
//! contract substrate's wasm runtime expects.
//!
//! No FFI imports are pulled in unconditionally — the host-fn externs
//! in [`raw`] live behind a `#[cfg(target_arch = "wasm32")]` block and
//! the native-target stubs panic if invoked, so the crate compiles
//! for `cargo test --workspace` on the host without dragging the FFI
//! surface into the linker.
//!
//! Original ADR coverage (history retained for the surfaces these
//! types still implement): ADR-0012 (typed sinks), ADR-0013 (reply-
//! to-sender), ADR-0014 (Component trait + Mail), ADR-0015 (lifecycle
//! hooks), ADR-0016 (state-across-replace), ADR-0024 (`_p32` FFI),
//! ADR-0030 (compile-time kind ids), ADR-0033 (`#[actor]`), ADR-0040
//! (kind-typed state), ADR-0041 (file I/O),
//! ADR-0043 (HTTP egress), ADR-0045 (typed handles), ADR-0058
//! (`aether.sink.*` namespace), ADR-0060 (tracing→mail bridge),
//! ADR-0074 (unified actor model).

use alloc::borrow::Cow;
use alloc::string::String;

use crate::actor::ctx::Resolver;
use core::fmt;

pub mod bridge;
pub mod ctx;
pub mod inline;
pub mod mailbox;
pub mod raw;

pub use ctx::{FfiCtx, FfiDropCtx, FfiInitCtx, SpawnError};
pub use mailbox::FfiActorMailbox;

// Issue 665 retired the `ffi::Mailbox<K>` 1-arg alias and the
// FFI-flavoured `resolve_mailbox` shim that pinned `T = FfiTransport`.
// The transport-free [`crate::mail::mailbox::Mailbox<K>`] is now the
// only `Mailbox` type; the crate-root [`crate::resolve_mailbox`]
// builds it directly.

/// Error returned by [`FfiActor::init`] when the actor cannot start
/// (config parse failure, required handle missing, malformed env var).
/// The message rides the `init_failed_p32` host fn into the substrate,
/// which surfaces it in `LoadResult::Err { error }` instead of the
/// panic-hook path's generic "guest trapped during init" text.
///
/// Wraps a `Cow<'static, str>` so static-string callers don't allocate
/// (`BootError::from("config missing")`) while owned strings still flow
/// through (`BootError::from(format!("..."))`).
#[derive(Debug, Clone)]
pub struct BootError {
    message: Cow<'static, str>,
}

impl BootError {
    /// Construct a `BootError` from anything convertible to a
    /// `Cow<'static, str>` — `&'static str` for compile-time messages,
    /// `String` for `format!`-built diagnostics.
    pub fn new<S: Into<Cow<'static, str>>>(message: S) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Borrow the error text. Used by the [`crate::export!`] shim to
    /// copy bytes into the substrate via `init_failed_p32`.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<&'static str> for BootError {
    fn from(s: &'static str) -> Self {
        Self::new(s)
    }
}

impl From<String> for BootError {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

/// User-implemented FFI actor — typically a wasm component. ADR-0014
/// commits to `Self`-is-state: cached kind ids, cached sinks, and any
/// domain fields live on the implementor. `init` runs once before any
/// `receive`; receive is driven by the synthesised `__aether_dispatch`
/// from `#[actor]`.
///
/// The [`crate::Actor`] super-trait owns the symmetric `NAMESPACE`
/// shared with the substrate-side `NativeActor`; `FfiActor` adds the
/// FFI lifecycle methods: the `init` constructor plus the
/// `wire` / `unwire` / `on_dehydrate` / `on_rehydrate` default-no-op
/// hooks, each overridden when an actor cares (ADR-0101).
///
/// The `#[no_mangle]` `init` / `receive` exports that actually cross
/// the FFI boundary are generated by `export!(MyComponent)`;
/// implementors do not write `extern "C"` by hand.
pub trait FfiActor: crate::Actor {
    /// ADR-0090 application-configuration: typed boot configuration
    /// the chassis threads through to [`Self::init`]. Mirrors
    /// `NativeActor::Config` so a single authoring shape crosses the
    /// wasm / native split (`#[actor] impl FfiActor` and
    /// `#[actor] impl NativeActor` both accept `type Config = …`).
    ///
    /// The `#[actor]` macro synthesizes `type Config = ();` when the
    /// user does not declare one — `impl Kind for ()` (in
    /// `aether-data`) lets the emitted FFI shim decode 0 config bytes
    /// through the uniform `<Self::Config as Kind>::decode_from_bytes`
    /// path. Stable Rust does not yet accept associated-type defaults
    /// (rust-lang/rust#29661); without the macro synthesis the user
    /// would have to write `type Config = ();` themselves.
    type Config: aether_data::Kind;

    /// ADR-0113 kind-typed persistent state: the durable shape the
    /// actor carries across a `replace_component` swap. Declaring it
    /// (beside a `dehydrate` / `rehydrate` accessor pair) lets the
    /// `#[actor]` macro generate the [`Self::on_dehydrate`] /
    /// [`Self::on_rehydrate`] hooks instead of the author hand-writing
    /// them — the save side snapshots `State` and frames it via
    /// `save_state_kind`, the restore side decodes it via
    /// [`PriorState::as_kind`][crate::PriorState::as_kind] and boots
    /// fresh (with a `tracing::warn!`) when a reshaped `State` kind no
    /// longer decodes.
    ///
    /// Mirrors [`Self::Config`]: the `#[actor]` macro synthesizes
    /// `type State = ();` when the author omits it, so a no-persistence
    /// actor is unchanged and pays nothing (stable Rust has no
    /// associated-type defaults — rust-lang/rust#29661 — so the
    /// synthesis stands in). Distinct from `Self` because the durable
    /// fields are a subset of the actor's state: the `Mailbox` tokens
    /// and handle ids `init` rebuilds are intentionally excluded
    /// (ADR-0113).
    type State: aether_data::Kind;

    /// Runs once. Resolve kinds and mailboxes via `ctx` and return the
    /// initial actor state. ADR-0033: `#[actor]` prepends
    /// `ctx.subscribe_input::<K>()` for every `K::IS_INPUT` handler
    /// kind so the user body never needs to do it by hand.
    ///
    /// Issue 663 phase D: the ctx parameter is generic — implementations
    /// program against the [`Resolver`] + [`MailSender`](crate::MailSender) trait surface
    /// rather than naming a concrete ctx type. The [`crate::export!`]
    /// macro constructs a [`FfiInitCtx`] and Rust infers `C` at the
    /// call site; user code never spells the ctx struct directly, so
    /// future hosts beyond wasm can be plugged in without touching the
    /// trait.
    ///
    /// ADR-0090: the typed `Self::Config` arrives as the leading
    /// parameter, symmetric with `aether_substrate::NativeActor::init`.
    /// Actors that opt out of configuration leave `Config = ()` (or omit
    /// it entirely, taking the trait default) and the `#[actor]` macro
    /// synthesizes the unused `_config: ()` argument so user bodies stay
    /// terse.
    ///
    /// Returns `Result<Self, BootError>` so an actor that hits an
    /// unrecoverable startup condition (config parse failure, required
    /// handle missing, malformed env var) can surface its own message
    /// in `LoadResult::Err { error }`.
    ///
    /// Issue 703: the bound is `C: Resolver` only — init is the sync
    /// constructor (ADR-0079) and must NOT mail. Use [`Self::wire`]
    /// for mail-driven setup (subscriptions, peer hellos, etc.).
    fn init<C>(config: Self::Config, ctx: &mut C) -> Result<Self, BootError>
    where
        Self: Sized,
        C: Resolver;

    /// Post-init mail-allowed hook (issue 584, ADR-0079 amended
    /// 2026-05-09). Runs after `init` returned `Ok` and the actor's
    /// mailbox is published, but before the dispatcher pulls the
    /// first envelope. The actor may send mail here — peers are
    /// addressable. Default no-op; override to register subscriptions,
    /// announce the actor, or kick off a poll loop via self-mail.
    ///
    /// Concrete `&mut FfiCtx<'_>` (mirrors native's
    /// `NativeActor::wire(&mut NativeCtx<'_>)`) so overrides reach for
    /// the inherent `ctx.actor::<R>().send(&payload)` shape directly.
    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        let _ = ctx;
    }

    /// Pre-shutdown mail-allowed hook (issue 584, ADR-0079 amended
    /// 2026-05-09). Runs after the dispatcher's inbox drain, before
    /// the actor value drops. Mail to live peers lands in their
    /// mailboxes; sends to a dead peer warn-drop. Default no-op;
    /// override to publish a final broadcast, signal monitors, or
    /// flush state.
    fn unwire(&mut self, ctx: &mut FfiCtx<'_>) {
        let _ = ctx;
    }

    /// Save-side hot-swap hook (ADR-0040 / ADR-0101). Runs once on the
    /// old instance immediately before a `replace_component` swap, after
    /// [`Self::unwire`]. Default no-op; override to serialize state the
    /// replacement instance recovers through [`Self::on_rehydrate`].
    /// Prefer
    /// [`FfiDropCtx::save_state_kind`][crate::actor::ctx::Persistence::save_state_kind]
    /// to let the kind system carry schema identity; reach for the raw
    /// [`FfiDropCtx::save_state`][crate::actor::ctx::Persistence::save_state]
    /// only when persisting a non-kind blob or driving an explicit
    /// migration off the leading id.
    ///
    /// Concrete `&mut FfiDropCtx<'_>` — the ctx that carries
    /// `Persistence::save_state` and outbound mail, with the reply /
    /// resolve surfaces intentionally absent.
    fn on_dehydrate(&mut self, ctx: &mut FfiDropCtx<'_>) {
        let _ = ctx;
    }

    /// Restore-side hot-swap hook (ADR-0040 / ADR-0101). Runs after
    /// [`Self::init`] on a freshly-instantiated replacement, if and only
    /// if the predecessor produced a state bundle via
    /// [`Self::on_dehydrate`] (the substrate skips the call when no
    /// bundle was saved — ADR-0016 §3). Default ignores the prior state;
    /// override to rehydrate from `prior` (typically
    /// [`PriorState::as_kind`][crate::PriorState::as_kind]).
    ///
    /// Concrete `&mut FfiCtx<'_>` — the post-init send surface, so an
    /// override can both restore fields and emit mail.
    fn on_rehydrate(&mut self, ctx: &mut FfiCtx<'_>, prior: crate::PriorState<'_>) {
        let _ = ctx;
        let _ = prior;
    }
}

/// Object-safe erasure over a guest [`FfiActor`]'s post-construction
/// surface (ADR-0096). A multi-actor module — `export!(A, B, …)` —
/// holds whichever exported type a given instance became in one
/// `Slot<Box<dyn ErasedFfiActor>>`, and the FFI shims route mail and
/// lifecycle calls through this trait. `#[actor]` emits the impl per
/// type, forwarding to the inherent `__aether_dispatch` and the
/// `FfiActor` lifecycle hooks.
///
/// `init` is deliberately not erased: it is generic over the ctx and
/// returns `Self`, so it cannot be a trait-object method. The
/// `export!` multi-actor arm matches the inbound actor-type tag against
/// each exported type and calls the concrete `T::init` before boxing
/// the result as a `dyn ErasedFfiActor`.
///
/// The hot-swap hooks erase the same way (ADR-0101), so a boxed
/// multi-actor instance preserves state across `replace_component`
/// with no multi-actor-specific machinery.
pub trait ErasedFfiActor {
    /// The actor type's [`crate::Actor::NAMESPACE`], so the `receive`
    /// shim can derive the instance's own mailbox id for self-addressing.
    fn erased_namespace(&self) -> &'static str;

    /// Forwards to the `#[actor]`-synthesized `__aether_dispatch`.
    /// ADR-0112: the object-safe seam carries the most-permissive
    /// [`Manual`](crate::Manual) view; the synthesized dispatcher
    /// downgrades per handler class.
    fn erased_dispatch(
        &mut self,
        ctx: &mut FfiCtx<'_, crate::Manual>,
        mail: crate::Mail<'_>,
    ) -> u32;

    /// Forwards to [`FfiActor::wire`] (the synthesized impl downgrades
    /// the carried [`Manual`](crate::Manual) ctx to `Single`).
    fn erased_wire(&mut self, ctx: &mut FfiCtx<'_, crate::Manual>);

    /// Forwards to [`FfiActor::unwire`].
    fn erased_unwire(&mut self, ctx: &mut FfiCtx<'_, crate::Manual>);

    /// Forwards to [`FfiActor::on_dehydrate`].
    fn erased_on_dehydrate(&mut self, ctx: &mut FfiDropCtx<'_>);

    /// Forwards to [`FfiActor::on_rehydrate`].
    fn erased_on_rehydrate(
        &mut self,
        ctx: &mut FfiCtx<'_, crate::Manual>,
        prior: crate::PriorState<'_>,
    );
}

/// Stage a guest init-failure message into the substrate via
/// `init_failed_p32` (ADR-0096). Shared by the multi-actor `export!`
/// init shims so the byte-staging boilerplate isn't repeated at each
/// construction site. wasm32-only — the host build carries no FFI
/// surface.
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub fn stage_init_failure(message: &str) {
    let bytes = message.as_bytes();
    // SAFETY: `init_failed` copies `len` bytes from `ptr` into the
    // substrate synchronously; the borrowed slice outlives the call.
    unsafe {
        raw::init_failed(bytes.as_ptr().addr() as u32, bytes.len() as u32);
    }
}

/// Generic guest allocator backing host→guest payload delivery (ADR-0095).
///
/// The substrate writes every inbound payload — mail, init config, rehydrate
/// state — into a region it obtains from this allocator, then calls the entry
/// point (`receive` / `init_with_config` / `on_rehydrate`). The host owns no
/// fixed offset into guest memory; the guest reports where its memory is, so
/// delivery is independent of the guest's linear-memory layout.
///
/// The export is `cabi_realloc`-shaped (the Component Model canonical): one
/// function that allocates (`old_ptr == 0`), grows possibly-relocating
/// (`new_size > old_size`), or frees (`new_size == 0`). The substrate drives it
/// under the run-token invariant — one small region allocated once and reused,
/// one large region grown to fit, each consumed synchronously — and never frees
/// (wasm has no `memory.shrink`, so a free reclaims nothing).
///
/// The first sub-threshold allocation — the small delivery region the host
/// obtains once at instantiate (`align = 8`, `new_size <= SMALL_REGION_BYTES`)
/// — is served from a reused `static` scratch (ADR-0095 point 4) rather than
/// the global allocator. Every later allocation (the large region, any
/// subsequent small one) falls through to the global allocator. The scratch is
/// purely a guest-side optimization beneath the same ABI; a guest in any
/// language supplies the same export however it likes.
///
/// The module is not target-gated so the allocator is host-testable (its body
/// is plain `alloc`-crate code, like [`crate::Slot`]); the `realloc_p32` FFI
/// shims that call it remain wasm32-only.
pub mod guest_alloc {
    use alloc::alloc::{Layout, alloc, dealloc, realloc};
    use core::cell::UnsafeCell;
    use core::ptr::{copy_nonoverlapping, null_mut};

    /// Size of the `static` scratch that backs the first sub-threshold
    /// delivery allocation. Kept `>=` the host's `SMALL_REGION_BYTES`
    /// (`aether-substrate`, currently 8 KiB) so the scratch covers that
    /// instantiate-time small region. If it ever drifts below the host floor
    /// the first small allocation simply spills to the global allocator —
    /// still correct, just no longer served from the scratch.
    const SCRATCH_REGION_BYTES: usize = 8 * 1024;

    /// Alignment the scratch satisfies. The host requests `align = 8` for the
    /// delivery region; a request needing more falls through to the global
    /// allocator even if it would fit by size.
    const SCRATCH_ALIGN: usize = 8;

    /// Reused backing store for the first small delivery region plus a
    /// single-shot claim flag. `UnsafeCell` + a blanket `Sync` impl mirrors
    /// [`crate::Slot`]: the guest is single-threaded (ADR-0010 §5) and the
    /// substrate serializes delivery under the run token, so the cell is never
    /// touched concurrently and no atomics are needed. `bytes` is the first
    /// field of a `repr(C, align(8))` struct, so its base is
    /// `SCRATCH_ALIGN`-aligned.
    #[repr(C, align(8))]
    struct Scratch {
        bytes: UnsafeCell<[u8; SCRATCH_REGION_BYTES]>,
        claimed: UnsafeCell<bool>,
    }

    // SAFETY: a single-threaded WASM guest plus the substrate's serialized,
    // run-token-gated delivery mean `SCRATCH` is only ever touched from one
    // thread at a time — the same argument that licenses `crate::Slot`'s
    // `Sync`. On the host unit-test build the static is reached from one test.
    unsafe impl Sync for Scratch {}

    static SCRATCH: Scratch = Scratch {
        bytes: UnsafeCell::new([0u8; SCRATCH_REGION_BYTES]),
        claimed: UnsafeCell::new(false),
    };

    /// `cabi_realloc`-shaped reallocation.
    ///
    /// - `old_ptr == 0` — allocate `new_size` bytes, return the pointer. The
    ///   first such call that fits the scratch (`new_size <=
    ///   SCRATCH_REGION_BYTES`, `align <= SCRATCH_ALIGN`) returns the reused
    ///   `static` scratch base; later calls use the global allocator.
    /// - `new_size == 0` — free the `(old_ptr, old_size, align)` allocation,
    ///   return null. Freeing the scratch base is a no-op.
    /// - otherwise — resize the allocation to `new_size`, returning the current
    ///   pointer (which differs from `old_ptr` if the block was relocated).
    ///
    /// # Safety
    /// `old_ptr` / `old_size` / `align` must describe a live allocation from a
    /// prior call (or `0` / `0` for a fresh allocation), and `align` must be a
    /// nonzero power of two. The guest is single-threaded, so there is no
    /// aliasing.
    pub unsafe fn realloc_bytes(
        old_ptr: *mut u8,
        old_size: usize,
        align: usize,
        new_size: usize,
    ) -> *mut u8 {
        let scratch_base = SCRATCH.bytes.get().cast::<u8>();
        if new_size == 0 {
            if !old_ptr.is_null() && old_ptr != scratch_base {
                // SAFETY: caller's layout contract holds (`align` a nonzero
                // power of two), and `old_ptr` is a live global allocation —
                // the scratch base is excluded above, so this only frees global
                // memory, never the `static` scratch.
                unsafe {
                    let layout = Layout::from_size_align_unchecked(old_size, align);
                    dealloc(old_ptr, layout);
                }
            }
            // Freeing the scratch base reclaims nothing (it is a `static`); the
            // host never frees the small region anyway (ADR-0095 point 6).
            return null_mut();
        }
        if old_ptr.is_null() {
            // SAFETY: single-threaded guest + serialized delivery — no other
            // live reference to the claim flag (the `Scratch` `Sync` argument).
            let already_claimed = unsafe { *SCRATCH.claimed.get() };
            if !already_claimed && new_size <= SCRATCH_REGION_BYTES && align <= SCRATCH_ALIGN {
                // SAFETY: same single-threaded / serialized argument; this is
                // the only writer of the flag, and it transitions once.
                unsafe { *SCRATCH.claimed.get() = true };
                return scratch_base;
            }
            // SAFETY: caller's layout contract holds; a fresh global allocation.
            return unsafe {
                let layout = Layout::from_size_align_unchecked(new_size, align);
                alloc(layout)
            };
        }
        if old_ptr == scratch_base {
            // Contractually unreachable — the host grows the large region, not
            // the scratch-backed small one (ADR-0095 points 4/7). Handle it
            // without UB by allocating fresh and copying rather than passing the
            // `static` pointer to `realloc`.
            // SAFETY: caller's layout contract holds; `scratch_base` is a live,
            // initialized `static` of `SCRATCH_REGION_BYTES` bytes, `fresh` is a
            // fresh global allocation of `new_size`, and the copy length is the
            // min of `old_size` / `new_size`, in-bounds of both regions.
            return unsafe {
                let layout = Layout::from_size_align_unchecked(new_size, align);
                let fresh = alloc(layout);
                if !fresh.is_null() {
                    copy_nonoverlapping(scratch_base, fresh, old_size.min(new_size));
                }
                fresh
            };
        }
        // SAFETY: caller's layout contract holds; `old_ptr` / `old_size` /
        // `align` describe a live global allocation (scratch base excluded
        // above), matched by the global `realloc`.
        unsafe {
            let old_layout = Layout::from_size_align_unchecked(old_size, align);
            realloc(old_ptr, old_layout, new_size)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The scratch serves the first eligible sub-threshold allocation and
        /// only that one; oversized and over-aligned requests bypass it, and a
        /// second eligible request falls through to the global allocator. Only
        /// this test claims the process-global single-shot flag, so the
        /// sequence is deterministic without a reset hook.
        #[test]
        fn scratch_serves_first_small_alloc_only() {
            let base = SCRATCH.bytes.get().cast::<u8>();
            let null = null_mut();
            // SAFETY: every call obeys `realloc_bytes`' contract — `null`/`0`
            // for fresh allocs, and a matching `(ptr, size, align)` triple to
            // free what a prior call returned. One test thread, no aliasing.
            unsafe {
                // Oversized: larger than the scratch → global allocator. The
                // flag stays unclaimed because the size gate fails.
                let big = realloc_bytes(null, 0, SCRATCH_ALIGN, SCRATCH_REGION_BYTES + 1);
                assert_ne!(big, base, "oversized alloc bypasses the scratch");
                realloc_bytes(big, SCRATCH_REGION_BYTES + 1, SCRATCH_ALIGN, 0);

                // Over-aligned: fits by size but needs more alignment than the
                // scratch provides → global allocator, flag still unclaimed.
                let aligned = realloc_bytes(null, 0, SCRATCH_ALIGN * 2, 64);
                assert_ne!(aligned, base, "over-aligned alloc bypasses the scratch");
                realloc_bytes(aligned, 64, SCRATCH_ALIGN * 2, 0);

                // First eligible small alloc (the host's instantiate-time small
                // region) → served from the scratch.
                let first = realloc_bytes(null, 0, SCRATCH_ALIGN, SCRATCH_REGION_BYTES);
                assert_eq!(
                    first, base,
                    "first eligible small alloc returns the scratch base"
                );

                // Single-shot: the next eligible alloc falls through to global.
                let second = realloc_bytes(null, 0, SCRATCH_ALIGN, 64);
                assert_ne!(second, base, "scratch is spent after the first claim");
                realloc_bytes(second, 64, SCRATCH_ALIGN, 0);
            }
        }

        /// Freeing the scratch base is a no-op that returns null and must not
        /// reach the global allocator's `dealloc` (UB on a `static`).
        #[test]
        fn freeing_scratch_base_is_a_noop() {
            let base = SCRATCH.bytes.get().cast::<u8>();
            // SAFETY: `base` is the scratch `static`; the free branch guards it
            // and never calls `dealloc` on it.
            let freed = unsafe { realloc_bytes(base, SCRATCH_REGION_BYTES, SCRATCH_ALIGN, 0) };
            assert!(freed.is_null(), "freeing the scratch returns null");
        }
    }
}

/// Bind a `FfiActor` implementor to the guest's `#[no_mangle]`
/// `init` / `receive` exports. Expands to:
///
/// - A `static` [`crate::Slot<T>`] that backs the actor instance.
/// - `extern "C" fn init(mailbox_id: u64) -> u32` — builds an
///   [`FfiInitCtx`], calls `T::init`, stashes the result in the slot.
/// - `extern "C" fn receive(kind, ptr, byte_len, count, sender, recipient)
///   -> u32` — builds [`FfiCtx`] and [`crate::Mail`], calls the
///   `#[actor]`-synthesized `__aether_dispatch` on the stashed
///   instance.
/// - `#[link_section = "aether.kinds.inputs"]` static that pins the
///   actor's handler manifest into the cdylib's wasm custom section
///   the substrate reads at `load_component`.
/// - `#[link_section = "aether.namespace"]` static that pins the
///   actor's `Actor::NAMESPACE` bytes (issue 525 Phase 1B).
///
/// A single-type `export!(C)` binds the shared `init` / `receive`
/// exports to one actor. ADR-0096 multi-actor modules pass two or more
/// types — `export!(First, Second, …)` — which routes through
/// `__export_multi_internal!`; the arity is what keeps the multi-actor
/// arm from shadowing this single-actor form.
///
/// ```ignore
/// pub struct Hello { /* fields */ }
/// impl aether_actor::FfiActor for Hello { /* init + receive */ }
/// aether_actor::export!(Hello);
/// ```
///
/// Hot-swap state continuity (ADR-0040 / ADR-0101) needs no flag: the
/// `on_dehydrate` / `on_rehydrate` exports always forward to the
/// `FfiActor` hooks, which default to no-ops unless the actor overrides
/// them.
#[macro_export]
macro_rules! export {
    ($component:ty) => {
        $crate::__export_internal!($component);
    };
    // ADR-0096: multi-actor module — two or more `FfiActor` types in one
    // crate. Requires at least a first + one more so it never shadows
    // the single-actor arm above.
    ($first:ty $(, $rest:ty)+ $(,)?) => {
        $crate::__export_multi_internal!(@entry $first ; @all $first $(, $rest)+);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_internal {
    ($component:ty) => {
        static __AETHER_COMPONENT: $crate::Slot<$component> = $crate::Slot::new();

        // ADR-0114: the component's own inline-child registry — one per
        // `export!`, mirroring `__AETHER_COMPONENT`. The `receive`
        // membrane and the dehydrate / rehydrate shims thread
        // `&__AETHER_INLINE` to the inline-child consumers instead of
        // reaching for a crate-global static.
        static __AETHER_INLINE: $crate::ffi::inline::InlineRegistry =
            $crate::ffi::inline::InlineRegistry::new();

        // ADR-0033 / issue 442: pin the actor's `aether.kinds.inputs`
        // bytes into the cdylib's wasm custom section. The const data
        // (`__AETHER_INPUTS_MANIFEST_LEN` / `__AETHER_INPUTS_MANIFEST`)
        // is emitted by `#[actor]` on the type's inherent impl;
        // section emission lives here so it only fires in the cdylib
        // root crate (where `export!()` is invoked) and never in
        // transitive rlib pulls of a `#[actor]`-using crate, which
        // would otherwise stack duplicate Component records and fail
        // the substrate's manifest reader.
        #[cfg(target_arch = "wasm32")]
        #[used]
        #[unsafe(link_section = "aether.kinds.inputs")]
        static __AETHER_INPUTS_SECTION: [u8; <$component>::__AETHER_INPUTS_MANIFEST_LEN] =
            <$component>::__AETHER_INPUTS_MANIFEST;

        // Issue 525 Phase 1B: pin the actor's `Actor::NAMESPACE` bytes
        // into a sibling `aether.namespace` custom section. The
        // substrate reads this at load time as the default mailbox
        // name when the load payload omits an explicit `name`.
        #[cfg(target_arch = "wasm32")]
        #[used]
        #[unsafe(link_section = "aether.namespace")]
        static __AETHER_NAMESPACE_SECTION: [u8; <$component as $crate::Actor>::NAMESPACE.len()] = {
            let bytes = <$component as $crate::Actor>::NAMESPACE.as_bytes();
            let mut out = [0u8; <$component as $crate::Actor>::NAMESPACE.len()];
            let mut i = 0;
            while i < bytes.len() {
                out[i] = bytes[i];
                i += 1;
            }
            out
        };

        /// # Safety
        /// Called exactly once by the substrate before any `receive`.
        /// Receives the actor's own mailbox id (ADR-0030 Phase 2) so
        /// `#[actor]`'s synthesized `init` prologue can self-address
        /// `subscribe_input` for every `K::IS_INPUT` handler kind.
        ///
        /// ADR-0090 (issue 1256): the substrate writes `config_len`
        /// bytes at `config_ptr` (`CONFIG_OFFSET` in the substrate's
        /// scratch layout) before calling. `config_len == 0` passes
        /// through as `&[]`, which decodes cleanly to `()` via
        /// `impl Kind for ()` for actors whose `Config` defaults to
        /// `()`. Typed-config actors expect a non-empty slice; a
        /// decode failure on either path stages the message via
        /// `init_failed_p32` and returns 1.
        ///
        /// Returns `0` on success and non-zero when the actor's `init`
        /// returned `Err(BootError)` or its `Config::decode_from_bytes`
        /// produced `None`.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(export_name = "init_with_config_p32")]
        pub unsafe extern "C" fn init_with_config(
            mailbox_id: u64,
            config_ptr: u32,
            config_len: u32,
        ) -> u32 {
            $crate::log::install_wasm_subscriber();
            // Build the config slice. Empty-len short-circuits to `&[]`
            // so a null/zero `config_ptr` is not dereferenced — mirrors
            // `PriorState::bytes`.
            let config_bytes: &[u8] = if config_len == 0 {
                &[]
            } else {
                // SAFETY: substrate wrote `config_len` bytes at
                // `config_ptr` (ADR-0090); slice lifetime is bounded
                // by this call, which finishes before the substrate
                // reuses the scratch region.
                unsafe {
                    ::core::slice::from_raw_parts(
                        config_ptr as usize as *const u8,
                        config_len as usize,
                    )
                }
            };
            let decoded = <<$component as $crate::FfiActor>::Config as $crate::__macro_internals::Kind>::decode_from_bytes(
                config_bytes,
            );
            let config = match decoded {
                ::core::option::Option::Some(c) => c,
                ::core::option::Option::None => {
                    let msg = ::core::concat!(
                        "guest init: ",
                        ::core::stringify!($component),
                        " could not decode Config from bytes",
                    );
                    let bytes = msg.as_bytes();
                    unsafe {
                        $crate::ffi::raw::init_failed(
                            bytes.as_ptr().addr() as u32,
                            bytes.len() as u32,
                        );
                    }
                    return 1;
                }
            };
            let mut ctx: $crate::FfiInitCtx<'_> = $crate::FfiInitCtx::__new(mailbox_id);
            match <$component as $crate::FfiActor>::init(config, &mut ctx) {
                Ok(instance) => {
                    unsafe {
                        __AETHER_COMPONENT.set(instance);
                    }
                    0
                }
                Err(err) => {
                    let msg = err.message();
                    let bytes = msg.as_bytes();
                    unsafe {
                        $crate::ffi::raw::init_failed(
                            bytes.as_ptr().addr() as u32,
                            bytes.len() as u32,
                        );
                    }
                    1
                }
            }
        }

        /// # Safety
        /// ADR-0090: legacy zero-config `init` shim. Called by older
        /// substrate builds that don't know about `init_with_config_p32`. Reaches
        /// into `init_with_config` with empty config bytes — works as long as
        /// `<Self as FfiActor>::Config` decodes from `&[]` (i.e.
        /// `Config = ()`); a typed-config actor returns an
        /// `init_failed` here, which is the right behavior for a host
        /// too old to thread config bytes through.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn init(mailbox_id: u64) -> u32 {
            // SAFETY: forwarding to `init_with_config` with `config_len = 0`
            // makes `config_ptr` unread (the function's empty-len
            // branch returns `&[]`), so the dummy `0` pointer is
            // never dereferenced.
            unsafe { init_with_config(mailbox_id, 0, 0) }
        }

        /// # Safety
        /// Called by the substrate exactly once after `init` returns
        /// Ok and the component's mailbox is published, before the
        /// first `receive` (issue 584 Phase 2b, ADR-0079 amended).
        /// Mail-allowed — peer mailboxes are addressable. Receives the
        /// component's own mailbox id so the SDK ctx can self-address.
        ///
        /// Issue 703: uses `FfiCtx` (Resolver + MailSender) so
        /// `Subscriber::subscribe_input::<K>()` resolves; `FfiInitCtx`
        /// is intentionally Resolver-only and can't mail.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn wire(mailbox_id: u64) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            // ADR-0112: the runtime builds the `Manual` view; `wire`'s
            // default signature is `FfiCtx<'_>` (= Single), so downgrade.
            let mut ctx = $crate::FfiCtx::__new(mailbox_id, &__AETHER_INLINE);
            <$component as $crate::FfiActor>::wire(instance, ctx.as_single());
            0
        }

        /// # Safety
        /// Called by the substrate exactly once before `on_dehydrate`
        /// (on a replace) or the instance drop, on the dying instance
        /// (issue 584 Phase 2b, ADR-0079 amended). Mail-allowed — live
        /// peers are still addressable; sends to a dead peer warn-drop.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn unwire(mailbox_id: u64) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx = $crate::FfiCtx::__new(mailbox_id, &__AETHER_INLINE);
            <$component as $crate::FfiActor>::unwire(instance, ctx.as_single());
            0
        }

        /// # Safety
        /// Called by the substrate with `(kind, ptr, byte_len, count,
        /// sender, recipient)` matching the FFI contract. Exported under
        /// the `_p32` suffix per ADR-0024 Phase 1; the trailing
        /// `recipient: u64` (ADR-0114 decision #1) widens like the other
        /// frame slots on the wasm path.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(export_name = "receive_p32")]
        pub unsafe extern "C" fn receive(
            kind: u64,
            ptr: u32,
            byte_len: u32,
            count: u32,
            sender: u32,
            recipient: u64,
        ) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            // Issue 703: derive the actor's own mailbox id at the
            // call site so `FfiCtx` can self-address (needed for
            // `Subscriber::subscribe_input::<K>()` from a handler).
            let mailbox_id = $crate::__macro_internals::mailbox_id_from_name(
                <$component as $crate::Actor>::NAMESPACE,
            )
            .0;
            let mail =
                unsafe { $crate::Mail::__from_raw(kind, ptr, byte_len, count, sender, recipient) };
            // ADR-0114: the receive membrane demuxes on the routed
            // recipient — own id dispatches the parent's handlers, an
            // inline-child alias dispatches the co-located child. For a
            // normally-addressed actor the recipient equals `mailbox_id`,
            // so the closure runs verbatim. ADR-0112: dispatch receives
            // the full `Manual` ctx; `__aether_dispatch` downgrades per
            // handler class.
            $crate::ffi::inline::membrane_dispatch(mailbox_id, mail, &__AETHER_INLINE, move |__aether_mail| {
                let mut ctx = $crate::FfiCtx::__new(mailbox_id, &__AETHER_INLINE);
                instance.__aether_dispatch(&mut ctx, __aether_mail)
            })
        }

        /// ADR-0095: the generic guest allocator the substrate delivers every
        /// inbound payload through. `cabi_realloc`-shaped — allocate
        /// (`old_ptr == 0`), grow possibly-relocating (`new_size > old_size`),
        /// free (`new_size == 0`). The substrate allocates a small region once
        /// at instantiate and a large region on demand, writes the payload, and
        /// calls the entry point (`receive` / `init_with_config` /
        /// `on_rehydrate`). Backed by [`$crate::ffi::guest_alloc`].
        ///
        /// # Safety
        /// Called by the substrate per the layout contract; see
        /// [`$crate::ffi::guest_alloc::realloc_bytes`].
        #[cfg(target_arch = "wasm32")]
        #[unsafe(export_name = "realloc_p32")]
        pub unsafe extern "C" fn realloc_p32(
            old_ptr: u32,
            old_size: u32,
            align: u32,
            new_size: u32,
        ) -> u32 {
            // SAFETY: see `guest_alloc::realloc_bytes`. wasm32 pointers are
            // 32-bit; a null result (free, or allocation failure) maps to 0.
            unsafe {
                $crate::ffi::guest_alloc::realloc_bytes(
                    old_ptr as *mut u8,
                    old_size as usize,
                    align as usize,
                    new_size as usize,
                )
                .addr() as u32
            }
        }

        /// # Safety
        /// Called by the substrate exactly once, on the old instance,
        /// immediately before a `replace_component` swap. Forwards to
        /// [`$crate::FfiActor::on_dehydrate`], a no-op unless the actor
        /// overrides it.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn on_dehydrate() -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            // Derive the actor's own mailbox id (its lineage carry) so a
            // `send::<R>` from the save hook resolves the receiver through
            // `R::resolve` — the same id `receive` derives for `FfiCtx`.
            let mailbox_id = $crate::__macro_internals::mailbox_id_from_name(
                <$component as $crate::Actor>::NAMESPACE,
            )
            .0;
            // ADR-0114 §5: run the parent's `on_dehydrate` and every
            // resident inline child's into a single composite, then call
            // the host `save_state` once. With no inline children the
            // composite is byte-identical to the parent's own blob, so a
            // childless component dehydrates exactly as before; a parent
            // that saves nothing and has no children skips the host save.
            if let Some((version, bytes)) = $crate::ffi::inline::compose::compose_dehydrate(
                mailbox_id,
                &__AETHER_INLINE,
                |ctx| <$component as $crate::FfiActor>::on_dehydrate(instance, ctx),
            ) {
                let mut ctx: $crate::FfiDropCtx<'_> = $crate::FfiDropCtx::__new(mailbox_id);
                ctx.save_state(version, &bytes);
            }
            0
        }

        /// # Safety
        /// Called by the substrate after `init` on a freshly
        /// instantiated replacement, with `(version, ptr, len)`
        /// describing the prior-state bundle the old instance produced.
        /// Exported under the `_p32` suffix per ADR-0024 Phase 1.
        /// Forwards to [`$crate::FfiActor::on_rehydrate`], a no-op unless
        /// the actor overrides it.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(export_name = "on_rehydrate_p32")]
        pub unsafe extern "C" fn on_rehydrate(version: u32, ptr: u32, len: u32) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mailbox_id = $crate::__macro_internals::mailbox_id_from_name(
                <$component as $crate::Actor>::NAMESPACE,
            )
            .0;
            // ADR-0114 §5: decompose the migration bundle, restore the
            // parent, then reconstruct each inline child by type. For a
            // childless component the bundle decomposes to the raw parent
            // blob, so the parent sees the identical `PriorState` it would
            // have before. A single-actor module's reconstructable type set
            // is just `$component` (an inline child of any other type is
            // not in the `export!` set; its tag is logged + skipped).
            let prior_bytes: &[u8] = if len == 0 {
                &[]
            } else {
                // SAFETY: substrate wrote `len` bytes at `ptr` (the rehydrate
                // ABI); the slice is bounded by this call.
                unsafe { ::core::slice::from_raw_parts(ptr as usize as *const u8, len as usize) }
            };
            $crate::ffi::inline::compose::reconstruct_inline_children(
                version,
                prior_bytes,
                &__AETHER_INLINE,
                |parent_version, parent_bytes| {
                    let mut ctx = $crate::FfiCtx::__new(mailbox_id, &__AETHER_INLINE);
                    // SAFETY: `parent_bytes` lives for this closure call;
                    // `PriorState::__from_ptr` bounds the slice to it.
                    let parent_prior = unsafe {
                        $crate::PriorState::__from_ptr(
                            parent_version,
                            parent_bytes.as_ptr() as usize,
                            parent_bytes.len(),
                        )
                    };
                    <$component as $crate::FfiActor>::on_rehydrate(
                        instance,
                        ctx.as_single(),
                        parent_prior,
                    );
                },
                |registry, child| {
                    $crate::__export_internal!(@reconstruct_child registry, child ; $component)
                },
            );
            0
        }
    };

    // Reconstruct one inline child by matching its persisted type tag
    // against the module's exported type set (ADR-0114 §5). For each
    // candidate type whose `hash(NAMESPACE)` matches, re-`init` it and
    // restore its state via `inline::compose::reconstruct_one_child`. An
    // unmatched tag returns `false` so the caller logs + skips it.
    (@reconstruct_child $registry:ident, $child:ident ; $($candidate:ty),+) => {{
        let mut __aether_reconstructed = false;
        $(
            if $child.type_tag
                == $crate::__macro_internals::mailbox_id_from_name(
                    <$candidate as $crate::Actor>::NAMESPACE,
                )
                .0
            {
                __aether_reconstructed =
                    $crate::ffi::inline::compose::reconstruct_one_child::<$candidate>(
                        $registry, $child,
                    );
            }
        )+
        __aether_reconstructed
    }};
}

/// ADR-0096: FFI shims for a multi-actor module — `export!(A, B, …)`.
///
/// One module-level `Slot<Box<dyn ErasedFfiActor>>` holds whichever
/// exported type the instance became. Two construction entry points:
///
/// - `init_with_config_p32` (the existing 3-arg ABI) constructs the
///   **entry** type (the first in the `export!` list). A host that
///   knows nothing about multi-actor modules loads the entry type with
///   no changes.
/// - `init_typed_p32` (4-arg, carries an actor-type tag) matches the
///   tag against each exported type's `mailbox_id_from_name(NAMESPACE)`
///   and constructs the selected one. The host calls this once it can
///   resolve an export selector to a tag (the follow-on PR).
///
/// `receive` / `wire` / `unwire` / `on_dehydrate` / `on_rehydrate` all
/// route through the boxed `ErasedFfiActor`, so a multi-actor instance
/// preserves state across `replace_component` exactly as a single-actor
/// one does (ADR-0101). The `aether.kinds.inputs` section carries every
/// exported type's records, each preceded by an `ActorBoundary`
/// (ADR-0096), so the host can regroup per type and resolve an export
/// selector to a tag. The `aether.namespace` section names the
/// **entry** type — the default mailbox name when the load omits both
/// an explicit name and an export selector.
#[doc(hidden)]
#[macro_export]
macro_rules! __export_multi_internal {
    (@entry $entry:ty ; @all $($component:ty),+) => {
        static __AETHER_MULTI: $crate::Slot<
            $crate::__macro_internals::Box<dyn $crate::ErasedFfiActor>
        > = $crate::Slot::new();

        // ADR-0114: the module's own inline-child registry — one per
        // `export!`, mirroring `__AETHER_MULTI`. The `receive` membrane
        // and the dehydrate / rehydrate shims thread `&__AETHER_INLINE` to
        // the inline-child consumers instead of reaching for a crate-global
        // static.
        static __AETHER_INLINE: $crate::ffi::inline::InlineRegistry =
            $crate::ffi::inline::InlineRegistry::new();

        // ADR-0096: per-actor `aether.kinds.inputs` section. Each
        // exported type's records are preceded by an
        // `ActorBoundary { namespace }` record (version-tagged like
        // every other record) so the host's
        // `read_actor_inputs_from_bytes` regroups the flat record
        // stream into one capability set per type. The entry type is
        // first. A single-actor `export!` never reaches this arm, so
        // the boundary-free single-actor layout stays byte-identical.
        #[cfg(target_arch = "wasm32")]
        const __AETHER_MULTI_INPUTS_LEN: usize = 0usize $(
            + 1
            + $crate::__macro_internals::canonical::inputs_actor_boundary_len(
                <$component as $crate::Actor>::NAMESPACE,
            )
            + <$component>::__AETHER_INPUTS_MANIFEST_LEN
        )+;

        #[cfg(target_arch = "wasm32")]
        #[used]
        #[unsafe(link_section = "aether.kinds.inputs")]
        static __AETHER_INPUTS_SECTION: [u8; __AETHER_MULTI_INPUTS_LEN] = {
            let mut out = [0u8; __AETHER_MULTI_INPUTS_LEN];
            let mut pos = 0usize;
            $(
                {
                    // The per-type `ActorBoundary` record, then that
                    // type's own `aether.kinds.inputs` manifest bytes.
                    const BOUNDARY_LEN: usize =
                        $crate::__macro_internals::canonical::inputs_actor_boundary_len(
                            <$component as $crate::Actor>::NAMESPACE,
                        );
                    const BOUNDARY_BYTES: [u8; BOUNDARY_LEN] =
                        $crate::__macro_internals::canonical::write_inputs_actor_boundary::<BOUNDARY_LEN>(
                            <$component as $crate::Actor>::NAMESPACE,
                        );
                    // Per-record section version byte, in lockstep with
                    // `INPUTS_SECTION_VERSION` (0x04, bumped by ADR-0112 /
                    // issue 1850; the multi-actor boundary record is a
                    // per-record frame and tracks the same version as the
                    // single-actor records emitted by the derive macro).
                    out[pos] = 0x04;
                    pos += 1;
                    let mut i = 0;
                    while i < BOUNDARY_LEN {
                        out[pos] = BOUNDARY_BYTES[i];
                        pos += 1;
                        i += 1;
                    }
                    const MANIFEST_LEN: usize = <$component>::__AETHER_INPUTS_MANIFEST_LEN;
                    const MANIFEST_BYTES: [u8; MANIFEST_LEN] =
                        <$component>::__AETHER_INPUTS_MANIFEST;
                    let mut j = 0;
                    while j < MANIFEST_LEN {
                        out[pos] = MANIFEST_BYTES[j];
                        pos += 1;
                        j += 1;
                    }
                }
            )+
            let _ = pos;
            out
        };

        #[cfg(target_arch = "wasm32")]
        #[used]
        #[unsafe(link_section = "aether.namespace")]
        static __AETHER_NAMESPACE_SECTION: [u8; <$entry as $crate::Actor>::NAMESPACE.len()] = {
            let bytes = <$entry as $crate::Actor>::NAMESPACE.as_bytes();
            let mut out = [0u8; <$entry as $crate::Actor>::NAMESPACE.len()];
            let mut i = 0;
            while i < bytes.len() {
                out[i] = bytes[i];
                i += 1;
            }
            out
        };

        /// # Safety
        /// Existing 3-arg init ABI; constructs the entry (first) export.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(export_name = "init_with_config_p32")]
        pub unsafe extern "C" fn init_with_config(
            mailbox_id: u64,
            config_ptr: u32,
            config_len: u32,
        ) -> u32 {
            $crate::log::install_wasm_subscriber();
            let config_bytes: &[u8] = if config_len == 0 {
                &[]
            } else {
                // SAFETY: substrate wrote `config_len` bytes at `config_ptr` (ADR-0090).
                unsafe {
                    ::core::slice::from_raw_parts(config_ptr as usize as *const u8, config_len as usize)
                }
            };
            $crate::__export_multi_internal!(@construct $entry, mailbox_id, config_bytes)
        }

        /// # Safety
        /// ADR-0090 legacy zero-config init; forwards to the entry type.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn init(mailbox_id: u64) -> u32 {
            unsafe { init_with_config(mailbox_id, 0, 0) }
        }

        /// # Safety
        /// ADR-0096 typed init: `type_tag` selects which exported type
        /// to construct (its `mailbox_id_from_name(NAMESPACE)`).
        #[cfg(target_arch = "wasm32")]
        #[unsafe(export_name = "init_typed_p32")]
        pub unsafe extern "C" fn init_typed(
            mailbox_id: u64,
            type_tag: u64,
            config_ptr: u32,
            config_len: u32,
        ) -> u32 {
            $crate::log::install_wasm_subscriber();
            let config_bytes: &[u8] = if config_len == 0 {
                &[]
            } else {
                // SAFETY: substrate wrote `config_len` bytes at `config_ptr` (ADR-0090).
                unsafe {
                    ::core::slice::from_raw_parts(config_ptr as usize as *const u8, config_len as usize)
                }
            };
            $(
                if type_tag
                    == $crate::__macro_internals::mailbox_id_from_name(
                        <$component as $crate::Actor>::NAMESPACE,
                    )
                    .0
                {
                    return $crate::__export_multi_internal!(@construct $component, mailbox_id, config_bytes);
                }
            )+
            $crate::ffi::stage_init_failure(
                "guest init: unknown actor-type tag for multi-actor module",
            );
            1
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn wire(mailbox_id: u64) -> u32 {
            let Some(instance) = (unsafe { __AETHER_MULTI.get_mut() }) else {
                return 1;
            };
            // ADR-0112: the boxed `ErasedFfiActor` seam carries the `Manual`
            // view; the synthesized impl downgrades to `Single` per hook.
            let mut ctx = $crate::FfiCtx::__new(mailbox_id, &__AETHER_INLINE);
            instance.erased_wire(&mut ctx);
            0
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn unwire(mailbox_id: u64) -> u32 {
            let Some(instance) = (unsafe { __AETHER_MULTI.get_mut() }) else {
                return 1;
            };
            // ADR-0112: the boxed `ErasedFfiActor` seam carries the `Manual`
            // view; the synthesized impl downgrades to `Single` per hook.
            let mut ctx = $crate::FfiCtx::__new(mailbox_id, &__AETHER_INLINE);
            instance.erased_unwire(&mut ctx);
            0
        }

        /// # Safety
        /// FFI receive contract (ADR-0024); routes through the boxed
        /// `ErasedFfiActor`. Self-mailbox id derived from the live
        /// instance's namespace. The trailing `recipient: u64` (ADR-0114
        /// decision #1) carries the routed mailbox through to `Mail`.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(export_name = "receive_p32")]
        pub unsafe extern "C" fn receive(
            kind: u64,
            ptr: u32,
            byte_len: u32,
            count: u32,
            sender: u32,
            recipient: u64,
        ) -> u32 {
            let Some(instance) = (unsafe { __AETHER_MULTI.get_mut() }) else {
                return 1;
            };
            let mailbox_id = $crate::__macro_internals::mailbox_id_from_name(
                instance.erased_namespace(),
            )
            .0;
            let mail =
                unsafe { $crate::Mail::__from_raw(kind, ptr, byte_len, count, sender, recipient) };
            // ADR-0114: same receive membrane as the single-actor arm —
            // own id dispatches the entry/boxed type, an inline-child
            // alias dispatches the co-located child. ADR-0112: the boxed
            // `ErasedFfiActor` seam carries the `Manual` view; the
            // synthesized impl downgrades to `Single` per hook.
            $crate::ffi::inline::membrane_dispatch(mailbox_id, mail, &__AETHER_INLINE, move |__aether_mail| {
                let mut ctx = $crate::FfiCtx::__new(mailbox_id, &__AETHER_INLINE);
                instance.erased_dispatch(&mut ctx, __aether_mail)
            })
        }

        /// ADR-0095 guest allocator — identical to the single-actor arm.
        ///
        /// # Safety
        /// Called by the substrate per the layout contract.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(export_name = "realloc_p32")]
        pub unsafe extern "C" fn realloc_p32(
            old_ptr: u32,
            old_size: u32,
            align: u32,
            new_size: u32,
        ) -> u32 {
            unsafe {
                $crate::ffi::guest_alloc::realloc_bytes(
                    old_ptr as *mut u8,
                    old_size as usize,
                    align as usize,
                    new_size as usize,
                )
                .addr() as u32
            }
        }

        /// # Safety
        /// Called by the substrate exactly once, on the old instance,
        /// immediately before a `replace_component` swap. Routes through
        /// the boxed `ErasedFfiActor` to the live type's
        /// [`$crate::FfiActor::on_dehydrate`] (ADR-0101).
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn on_dehydrate() -> u32 {
            let Some(instance) = (unsafe { __AETHER_MULTI.get_mut() }) else {
                return 1;
            };
            // Derive the live actor's own mailbox id (its lineage carry) so a
            // `send::<R>` from the save hook resolves the receiver through
            // `R::resolve` — the same id `receive` derives for `FfiCtx`.
            let mailbox_id = $crate::__macro_internals::mailbox_id_from_name(
                instance.erased_namespace(),
            )
            .0;
            // ADR-0114 §5: compose the parent + every inline child into one
            // composite, then `save_state` once (the boxed instance's
            // dehydrate routes through `erased_on_dehydrate`). Childless ⇒
            // byte-identical to the boxed parent's own blob.
            if let Some((version, bytes)) = $crate::ffi::inline::compose::compose_dehydrate(
                mailbox_id,
                &__AETHER_INLINE,
                |ctx| instance.erased_on_dehydrate(ctx),
            ) {
                let mut ctx: $crate::FfiDropCtx<'_> = $crate::FfiDropCtx::__new(mailbox_id);
                ctx.save_state(version, &bytes);
            }
            0
        }

        /// # Safety
        /// Called by the substrate after `init` on a freshly
        /// instantiated replacement, with `(version, ptr, len)`
        /// describing the prior-state bundle the old instance produced.
        /// Routes through the boxed `ErasedFfiActor` to the live type's
        /// [`$crate::FfiActor::on_rehydrate`] (ADR-0101). Self-mailbox id
        /// derived from the live instance's namespace.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(export_name = "on_rehydrate_p32")]
        pub unsafe extern "C" fn on_rehydrate(version: u32, ptr: u32, len: u32) -> u32 {
            let Some(instance) = (unsafe { __AETHER_MULTI.get_mut() }) else {
                return 1;
            };
            let mailbox_id = $crate::__macro_internals::mailbox_id_from_name(
                instance.erased_namespace(),
            )
            .0;
            // ADR-0114 §5: decompose, restore the boxed parent, then
            // reconstruct each inline child by matching its type tag against
            // every exported type. Childless ⇒ the boxed parent sees the
            // identical `PriorState`.
            let prior_bytes: &[u8] = if len == 0 {
                &[]
            } else {
                // SAFETY: substrate wrote `len` bytes at `ptr` (the rehydrate
                // ABI); the slice is bounded by this call.
                unsafe { ::core::slice::from_raw_parts(ptr as usize as *const u8, len as usize) }
            };
            $crate::ffi::inline::compose::reconstruct_inline_children(
                version,
                prior_bytes,
                &__AETHER_INLINE,
                |parent_version, parent_bytes| {
                    // ADR-0112: the boxed `ErasedFfiActor` seam carries the
                    // `Manual` view; the synthesized impl downgrades per hook.
                    let mut ctx = $crate::FfiCtx::__new(mailbox_id, &__AETHER_INLINE);
                    // SAFETY: `parent_bytes` lives for this closure call.
                    let parent_prior = unsafe {
                        $crate::PriorState::__from_ptr(
                            parent_version,
                            parent_bytes.as_ptr() as usize,
                            parent_bytes.len(),
                        )
                    };
                    instance.erased_on_rehydrate(&mut ctx, parent_prior);
                },
                |registry, child| {
                    $crate::__export_internal!(@reconstruct_child registry, child ; $($component),+)
                },
            );
            0
        }
    };

    // Decode `$ty`'s Config from `$config_bytes`, run its `init`, and box
    // the result into `__AETHER_MULTI`. Expands inline in an init shim;
    // a decode/init failure stages the message and `return 1`.
    (@construct $ty:ty, $mailbox_id:ident, $config_bytes:ident) => {{
        let config = match <
            <$ty as $crate::FfiActor>::Config as $crate::__macro_internals::Kind
        >::decode_from_bytes($config_bytes) {
            ::core::option::Option::Some(c) => c,
            ::core::option::Option::None => {
                $crate::ffi::stage_init_failure(::core::concat!(
                    "guest init: ",
                    ::core::stringify!($ty),
                    " could not decode Config from bytes",
                ));
                return 1;
            }
        };
        let mut ctx: $crate::FfiInitCtx<'_> = $crate::FfiInitCtx::__new($mailbox_id);
        match <$ty as $crate::FfiActor>::init(config, &mut ctx) {
            ::core::result::Result::Ok(instance) => {
                unsafe {
                    __AETHER_MULTI.set(
                        $crate::__macro_internals::Box::new(instance)
                            as $crate::__macro_internals::Box<dyn $crate::ErasedFfiActor>,
                    );
                }
                0
            }
            ::core::result::Result::Err(err) => {
                $crate::ffi::stage_init_failure(err.message());
                1
            }
        }
    }};
}
