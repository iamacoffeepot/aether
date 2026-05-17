//! FFI-actor binding layer. The contract: any host that exposes the
//! `_p32`-suffixed import surface (today: the wasm runtime in
//! `aether-substrate::actor::wasm`; future: a C host, an OS-process
//! host, ...) can drive an actor through this module.
//!
//! Surface:
//!
//!   - [`raw`] — `extern "C"` host-fn imports + host-target panic
//!     stubs (the only place the `_p32` symbols are named).
//!   - [`bridge`] — per-concern ZST dispatch surfaces ([`MAIL_BRIDGE`],
//!     [`PERSIST_BRIDGE`], [`SYNC_WAIT_BRIDGE`]). Each ZST owns one FFI op family
//!     and forwards inherent methods to the matching [`raw`]`::*`
//!     host fn. Issue 665 split the prior monolithic
//!     `MailTransport`-impl ZST into these per-concern bridges so
//!     persistence isn't mixed with mail and sync-wait isn't mixed
//!     with either.
//!   - [`FfiInitCtx`] / [`FfiCtx`] / [`FfiDropCtx`] — concrete per-stage
//!     ctx structs, each impling the relevant subset of the per-stage
//!     capability traits in [`crate::actor::ctx`].
//!   - [`FfiActorMailbox<R>`] — actor-typed sender returned by
//!     `ctx.actor::<R>()` / `ctx.resolve_actor::<R>(name)`. Lifetime-
//!     free — the global [`MAIL_BRIDGE`] static covers dispatch.
//!   - [`FfiActor`] trait — entry point with `init` and `on_drop`
//!     hooks. `init` returns `Result<Self, BootError>` so a guest can
//!     surface its own error message instead of the panic-hook path's
//!     generic "guest trapped during init" text.
//!   - [`Replaceable`] — opt-in hot-swap hooks (ADR-0040
//!     `replace_component`).
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
//! (kind-typed state), ADR-0041 (file I/O), ADR-0042 (sync `wait_reply`),
//! ADR-0043 (HTTP egress), ADR-0045 (typed handles), ADR-0058
//! (`aether.sink.*` namespace), ADR-0060 (tracing→mail bridge),
//! ADR-0074 (unified actor model).

use alloc::borrow::Cow;
use alloc::string::String;

use crate::actor::ctx::{MailSender, OutboundReply, Persistence, Resolver};

pub mod bridge;
pub mod ctx;
pub mod mailbox;
pub mod raw;

pub use bridge::{
    MAIL_BRIDGE, MailBridge, PERSIST_BRIDGE, PersistBridge, SYNC_WAIT_BRIDGE, SyncWaitBridge,
};
pub use ctx::{FfiCtx, FfiDropCtx, FfiInitCtx};
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

impl core::fmt::Display for BootError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
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
/// Issue 525 Phase 4 split the trait surface: the [`crate::Actor`]
/// super-trait owns the symmetric bits (`NAMESPACE`, `FRAME_BARRIER`)
/// shared with the substrate-side `NativeActor`; `FfiActor` adds the
/// FFI lifecycle methods (`init`, `wire`, `unwire`). Hot-swap hooks
/// (`on_replace`, `on_rehydrate`) moved to the opt-in [`Replaceable`]
/// sub-trait. (Issue 584 Phase 3 retired `on_drop` — `unwire` now
/// covers the pre-shutdown mail-allowed cleanup role.)
///
/// The `#[no_mangle]` `init` / `receive` exports that actually cross
/// the FFI boundary are generated by `export!(MyComponent)`;
/// implementors do not write `extern "C"` by hand.
pub trait FfiActor: crate::Actor {
    /// Runs once. Resolve kinds and mailboxes via `ctx` and return the
    /// initial actor state. ADR-0033: `#[actor]` prepends
    /// `ctx.subscribe_input::<K>()` for every `K::IS_INPUT` handler
    /// kind so the user body never needs to do it by hand.
    ///
    /// Issue 663 phase D: the ctx parameter is generic — implementations
    /// program against the [`Resolver`] + [`MailSender`] trait surface
    /// rather than naming a concrete ctx type. The [`crate::export!`]
    /// macro constructs a [`FfiInitCtx`] and Rust infers `C` at the
    /// call site; user code never spells the ctx struct directly, so
    /// future hosts beyond wasm can be plugged in without touching the
    /// trait.
    ///
    /// Returns `Result<Self, BootError>` so an actor that hits an
    /// unrecoverable startup condition (config parse failure, required
    /// handle missing, malformed env var) can surface its own message
    /// in `LoadResult::Err { error }`.
    ///
    /// Issue 703: the bound is `C: Resolver` only — init is the sync
    /// constructor (ADR-0079) and must NOT mail. Use [`Self::wire`]
    /// for mail-driven setup (subscriptions, peer hellos, etc.).
    fn init<C>(ctx: &mut C) -> Result<Self, BootError>
    where
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
    fn wire(&mut self, ctx: &mut crate::ffi::ctx::FfiCtx<'_>) {
        let _ = ctx;
    }

    /// Pre-shutdown mail-allowed hook (issue 584, ADR-0079 amended
    /// 2026-05-09). Runs after the dispatcher's inbox drain, before
    /// the actor value drops. Mail to live peers lands in their
    /// mailboxes; sends to a dead peer warn-drop. Default no-op;
    /// override to publish a final broadcast, signal monitors, or
    /// flush state.
    fn unwire(&mut self, ctx: &mut crate::ffi::ctx::FfiCtx<'_>) {
        let _ = ctx;
    }
}

/// Opt-in trait for actors that participate in hot-swap (ADR-0040
/// `replace_component`). Actors that don't impl `Replaceable` behave
/// as if both methods were no-ops — the FFI shim emitted by the
/// default [`crate::export!`] macro returns `0` for `on_replace` /
/// `on_rehydrate` without dispatching into the trait. To wire the
/// hooks, declare `impl Replaceable for X` and emit via
/// `aether_actor::export!(X, replaceable)`.
pub trait Replaceable: FfiActor {
    /// Called once on the old instance, immediately before a
    /// `replace_component` swap (ADR-0015 §3). Default is no-op;
    /// override to serialize state that the new instance can consume
    /// through `on_rehydrate`, or to emit farewell mail. Prefer
    /// [`FfiDropCtx::save_state_kind`][crate::actor::ctx::Persistence::save_state_kind]
    /// to let the kind system carry schema identity; reach for the
    /// raw [`FfiDropCtx::save_state`][crate::actor::ctx::Persistence::save_state]
    /// only when persisting a non-kind blob or driving an explicit
    /// migration off the leading id.
    fn on_replace<C>(&mut self, ctx: &mut C)
    where
        C: MailSender + Persistence,
    {
        let _ = ctx;
    }

    /// Called after `init` on a freshly-instantiated actor that is
    /// replacing an older instance, if and only if the predecessor
    /// produced a state bundle via the `Persistence` trait's
    /// `save_state` / `save_state_kind`. Default ignores the prior
    /// state.
    fn on_rehydrate<C>(&mut self, ctx: &mut C, prior: crate::PriorState<'_>)
    where
        C: OutboundReply,
    {
        let _ = ctx;
        let _ = prior;
    }
}

/// Bind a `FfiActor` implementor to the guest's `#[no_mangle]`
/// `init` / `receive` exports. Expands to:
///
/// - A `static` [`crate::Slot<T>`] that backs the actor instance.
/// - `extern "C" fn init(mailbox_id: u64) -> u32` — builds an
///   [`FfiInitCtx`], calls `T::init`, stashes the result in the slot.
/// - `extern "C" fn receive(kind, ptr, byte_len, count, sender) -> u32`
///   — builds [`FfiCtx`] and [`crate::Mail`], calls the
///   `#[actor]`-synthesized `__aether_dispatch` on the stashed
///   instance.
/// - `#[link_section = "aether.kinds.inputs"]` static that pins the
///   actor's handler manifest into the cdylib's wasm custom section
///   the substrate reads at `load_component`.
/// - `#[link_section = "aether.namespace"]` static that pins the
///   actor's `Actor::NAMESPACE` bytes (issue 525 Phase 1B).
///
/// Only one actor per guest crate. A second [`crate::export!`] call in
/// the same crate is a duplicate-symbol compile error on the shared
/// `init` / `receive` names — ADR-0014 §4 parks multi-actor crates as
/// out of scope.
///
/// ```ignore
/// pub struct Hello { /* fields */ }
/// impl aether_actor::FfiActor for Hello { /* init + receive */ }
/// aether_actor::export!(Hello);
/// ```
///
/// Actors that participate in hot-swap (ADR-0040) opt in via the
/// `replaceable` flag — `aether_actor::export!(Hello, replaceable);`.
#[macro_export]
macro_rules! export {
    ($component:ty) => {
        $crate::__export_internal!($component, no_replaceable);
    };
    ($component:ty, replaceable) => {
        $crate::__export_internal!($component, replaceable);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_internal {
    ($component:ty, $replaceable:ident) => {
        static __AETHER_COMPONENT: $crate::Slot<$component> = $crate::Slot::new();

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
        static __AETHER_NAMESPACE_SECTION: [u8; <$component as $crate::Actor>::NAMESPACE
            .len()] = {
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
        /// Returns `0` on success and non-zero when the actor's `init`
        /// returned `Err(BootError)`. On the `Err` path the shim ships
        /// the error text to the substrate via the `init_failed_p32`
        /// host fn before returning, so the substrate surfaces the
        /// message in `LoadResult::Err`.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn init(mailbox_id: u64) -> u32 {
            $crate::log::install_wasm_subscriber();
            let mut ctx: $crate::FfiInitCtx<'_> = $crate::FfiInitCtx::__new(mailbox_id);
            let status = match <$component as $crate::FfiActor>::init(&mut ctx) {
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
            };
            // Issue #598: flush any tracing events emitted during init
            // so the substrate can surface them.
            $crate::log::drain_buffer();
            status
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
            let mut ctx: $crate::FfiCtx<'_> = $crate::FfiCtx::__new(mailbox_id);
            <$component as $crate::FfiActor>::wire(instance, &mut ctx);
            $crate::log::drain_buffer();
            0
        }

        /// # Safety
        /// Called by the substrate exactly once before `on_drop` /
        /// `on_replace` on the dying instance (issue 584 Phase 2b,
        /// ADR-0079 amended). Mail-allowed — live peers are still
        /// addressable; sends to a dead peer warn-drop.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn unwire(mailbox_id: u64) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx: $crate::FfiCtx<'_> = $crate::FfiCtx::__new(mailbox_id);
            <$component as $crate::FfiActor>::unwire(instance, &mut ctx);
            $crate::log::drain_buffer();
            0
        }

        /// # Safety
        /// Called by the substrate with `(kind, ptr, byte_len, count,
        /// sender)` matching the FFI contract. Exported under the
        /// `_p32` suffix per ADR-0024 Phase 1.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(export_name = "receive_p32")]
        pub unsafe extern "C" fn receive(
            kind: u64,
            ptr: u32,
            byte_len: u32,
            count: u32,
            sender: u32,
        ) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            // Issue 703: derive the actor's own mailbox id at the
            // call site so `FfiCtx` can self-address (needed for
            // `Subscriber::subscribe_input::<K>()` from a handler).
            let mailbox_id = $crate::__macro_internals::mailbox_id_from_name(
                <$component as $crate::Actor>::NAMESPACE,
            ).0;
            let mut ctx: $crate::FfiCtx<'_> = $crate::FfiCtx::__new(mailbox_id);
            let mail = unsafe { $crate::Mail::__from_raw(kind, ptr, byte_len, count, sender) };
            let status = instance.__aether_dispatch(&mut ctx, mail);
            // Issue #598: ship buffered tracing events at handler exit.
            $crate::log::drain_buffer();
            status
        }

        /// # Safety
        /// Called by the substrate exactly once, on the old instance,
        /// immediately before a `replace_component` swap.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn on_replace() -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx: $crate::FfiDropCtx<'_> = $crate::FfiDropCtx::__new();
            $crate::__export_internal!(@on_replace $replaceable, $component, instance, ctx);
            $crate::log::drain_buffer();
            0
        }

        /// # Safety
        /// Called by the substrate after `init` on a freshly
        /// instantiated replacement, with `(version, ptr, len)`
        /// describing the prior-state bundle the old instance produced.
        /// Exported under the `_p32` suffix per ADR-0024 Phase 1.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(export_name = "on_rehydrate_p32")]
        pub unsafe extern "C" fn on_rehydrate(version: u32, ptr: u32, len: u32) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mailbox_id = $crate::__macro_internals::mailbox_id_from_name(
                <$component as $crate::Actor>::NAMESPACE,
            ).0;
            let mut ctx: $crate::FfiCtx<'_> = $crate::FfiCtx::__new(mailbox_id);
            let prior = unsafe { $crate::PriorState::__from_raw(version, ptr, len) };
            $crate::__export_internal!(@on_rehydrate $replaceable, $component, instance, ctx, prior);
            $crate::log::drain_buffer();
            0
        }
    };

    // Internal: per-replaceable-flag dispatch arms for the FFI shims.
    // The default `no_replaceable` arm leaves the FFI bodies empty
    // (substrate still calls but no work runs); the `replaceable` arm
    // dispatches into the actor's `Replaceable` impl.
    (@on_replace no_replaceable, $component:ty, $instance:ident, $ctx:ident) => {
        // No-op: actor opted out of hot-swap.
        let _ = (&$instance, &$ctx);
    };
    (@on_replace replaceable, $component:ty, $instance:ident, $ctx:ident) => {
        <$component as $crate::Replaceable>::on_replace($instance, &mut $ctx);
    };
    (@on_rehydrate no_replaceable, $component:ty, $instance:ident, $ctx:ident, $prior:ident) => {
        let _ = (&$instance, &$ctx, &$prior);
    };
    (@on_rehydrate replaceable, $component:ty, $instance:ident, $ctx:ident, $prior:ident) => {
        <$component as $crate::Replaceable>::on_rehydrate($instance, &mut $ctx, $prior);
    };
}
