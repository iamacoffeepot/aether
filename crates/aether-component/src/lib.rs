//! aether-component: WASM-guest facade over the actor SDK.
//!
//! Post-ADR-0074 Phase 1, the SDK types (`MailTransport`, `Mail`,
//! `Ctx`, `Sink`, `InitCtx`, `DropCtx`, `Slot`, `WaitError`, the
//! `wait_reply` helper, and the typed-handle module) live in
//! `aether-actor`. This crate keeps the wasm-specific shim:
//!
//!   - [`raw`] — `extern "C"` host-fn imports + host-target panic
//!     stubs (the only place the `_p32` symbols are named).
//!   - [`WasmTransport`] — ZST that implements
//!     `aether_actor::MailTransport` by delegating each method to the
//!     matching `raw::*` host fn.
//!   - 1-arg type aliases — `Sink<K>` = `aether_actor::Sink<K,
//!     WasmTransport>`, `Ctx<'a>` = `aether_actor::Ctx<'a,
//!     WasmTransport>`, etc. Existing component code keeps writing
//!     the un-parameterised types; the alias pins `T = WasmTransport`
//!     for the wasm guest path.
//!   - [`Component`] trait — wasm-flavoured entry point. Methods take
//!     the aliased types, so user impls compile unchanged. A
//!     companion native-actor trait will land in `aether-substrate`
//!     when ADR-0074 Phase 2 introduces `NativeTransport`.
//!   - [`io`], [`net`], [`log`] — wasm-specific helper modules. They
//!     specialise `aether_actor::wait_reply` to `WasmTransport`
//!     internally so the existing `io::read_sync(...)` /
//!     `net::fetch_blocking(...)` call shapes don't grow turbofish.
//!     `log` installs the global `tracing` default the `export!`
//!     macro plumbs into the `init` shim.
//!   - [`export!`] — `#[no_mangle]` `init` / `receive` / lifecycle
//!     shims plus the `aether.kinds.inputs` custom-section pin (issue
//!     442). Builds `WasmTransport`-flavoured `InitCtx`/`Ctx`/`DropCtx`
//!     for the user's `Component` impl.
//!
//! Original ADR coverage (history retained for the surfaces the
//! moved types still implement): ADR-0012 (typed sinks), ADR-0013
//! (reply-to-sender), ADR-0014 (Component trait + Mail), ADR-0015
//! (lifecycle hooks), ADR-0016 (state-across-replace), ADR-0024
//! (`_p32` FFI), ADR-0030 (compile-time kind ids), ADR-0033
//! (`#[actor]`), ADR-0040 (kind-typed state), ADR-0041 (file
//! I/O), ADR-0042 (sync wait_reply), ADR-0043 (HTTP egress),
//! ADR-0045 (typed handles), ADR-0058 (`aether.sink.*` namespace),
//! ADR-0060 (tracing→mail bridge), ADR-0074 (this restructure).

#![no_std]

extern crate alloc;

use aether_actor::MailTransport;
use alloc::borrow::Cow;
use alloc::string::String;

/// Error returned by [`WasmActor::init`] when the component cannot
/// start (config parse failure, required handle missing, malformed
/// env var). The message rides the `init_failed_p32` host fn into
/// the substrate, which surfaces it in `LoadResult::Err { error }`
/// instead of the panic-hook path's generic "guest trapped during
/// init" text. Issue 525 Phase 4b / issue 531.
///
/// Wraps a `Cow<'static, str>` so static-string callers don't
/// allocate (`BootError::from("config missing")`) while owned
/// strings still flow through (`BootError::from(format!("..."))`).
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

    /// Borrow the error text. Used by the `export!` shim to copy
    /// bytes into the substrate via `init_failed_p32`.
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

pub mod io;
pub mod log;
pub mod net;
pub mod raw;

/// ZST `MailTransport` impl for the WASM guest path. Each method
/// forwards to the matching `raw::*` host-fn import. The `&self`
/// receiver is unused — `WasmTransport` carries no per-instance
/// state because the FFI imports are global to the wasm instance —
/// so there's no overhead beyond the host-fn call itself.
///
/// `aether_substrate::NativeTransport` is the native counterpart;
/// both impls share the same SDK in `aether-actor` and the same
/// trait surface.
pub struct WasmTransport;

/// Process-wide `WasmTransport` instance. The type is a ZST, so
/// this `static` occupies zero bytes; its only purpose is giving
/// `&WASM_TRANSPORT` callers (the `export!`-emitted Ctx/InitCtx/
/// DropCtx constructors, the `io` / `net` / `log` helper modules,
/// component examples) a stable address to borrow without each
/// call site having to write `&WasmTransport` inline.
pub static WASM_TRANSPORT: WasmTransport = WasmTransport;

impl MailTransport for WasmTransport {
    fn send_mail(&self, recipient: u64, kind: u64, bytes: &[u8], count: u32) -> u32 {
        unsafe {
            raw::send_mail(
                recipient,
                kind,
                bytes.as_ptr().addr() as u32,
                bytes.len() as u32,
                count,
            )
        }
    }

    fn reply_mail(&self, sender: u32, kind: u64, bytes: &[u8], count: u32) -> u32 {
        unsafe {
            raw::reply_mail(
                sender,
                kind,
                bytes.as_ptr().addr() as u32,
                bytes.len() as u32,
                count,
            )
        }
    }

    fn save_state(&self, version: u32, bytes: &[u8]) -> u32 {
        unsafe { raw::save_state(version, bytes.as_ptr().addr() as u32, bytes.len() as u32) }
    }

    fn wait_reply(
        &self,
        expected_kind: u64,
        out: &mut [u8],
        timeout_ms: u32,
        expected_correlation: u64,
    ) -> i32 {
        unsafe {
            raw::wait_reply(
                expected_kind,
                out.as_mut_ptr().addr() as u32,
                out.len() as u32,
                timeout_ms,
                expected_correlation,
            )
        }
    }

    fn prev_correlation(&self) -> u64 {
        unsafe { raw::prev_correlation() }
    }
}

// 1-arg specialisations of the actor SDK's transport-generic types.
// Component code writes `Mailbox<MyKind>`, `Ctx<'_>`, `InitCtx<'_>`,
// `DropCtx<'_>` — the alias pins `T = WasmTransport`.

/// Wasm-flavoured [`aether_actor::Mailbox`].
pub type Mailbox<K> = aether_actor::Mailbox<K, WasmTransport>;
/// Wasm-flavoured [`aether_actor::Ctx`].
pub type Ctx<'a> = aether_actor::Ctx<'a, WasmTransport>;
/// Wasm-flavoured [`aether_actor::InitCtx`].
pub type InitCtx<'a> = aether_actor::InitCtx<'a, WasmTransport>;
/// Wasm-flavoured [`aether_actor::DropCtx`].
pub type DropCtx<'a> = aether_actor::DropCtx<'a, WasmTransport>;
/// Wasm-flavoured [`aether_actor::handle::Handle`]. Re-exposed at the
/// crate root so existing `aether_component::Handle<MyKind>` paths
/// keep resolving.
pub type Handle<K> = aether_actor::handle::Handle<K, WasmTransport>;

// Re-exports — these types have no transport coupling, so they pass
// through unchanged from the SDK.
pub use aether_actor::{
    DISPATCH_HANDLED, DISPATCH_UNKNOWN_KIND, HandlesKind, KindId, Mail,
    MailTransport as MailTransportTrait, NO_REPLY_HANDLE, PriorState, ReplyTo, Singleton, Slot,
    WaitError, decode_wait_reply, resolve, wait_reply,
};

/// Wasm-flavoured `resolve_mailbox` — pins `T = WasmTransport` so the
/// 1-turbofish-arg call shape `resolve_mailbox::<MyKind>(name)` works
/// without spelling out the transport. The transport-generic version
/// is at [`aether_actor::resolve_mailbox`] for hand-rolled callers
/// that want a non-wasm transport.
pub const fn resolve_mailbox<K: aether_data::Kind>(mailbox_name: &str) -> Mailbox<K> {
    aether_actor::resolve_mailbox::<K, WasmTransport>(mailbox_name)
}
/// Re-export the typed-handle module path so existing
/// `aether_component::handle::publish` callers compile unchanged.
pub use aether_actor::handle;
pub use aether_actor::handle::SyncHandleError;

/// ADR-0033 attribute macros. Applied to `impl Component for C`
/// blocks: `#[actor]` at the impl level; `#[handler]` on each typed
/// handler method; `#[fallback]` on an optional catchall. Forwarded
/// from `aether-data-derive` so the full component vocabulary sits
/// behind one `use aether_component::*` line.
pub use aether_data::{actor, fallback, handler};

/// Re-exports the `#[actor]` macro relies on at expansion sites
/// that don't depend on `aether-data` directly. The macro emits
/// `::aether_component::__macro_internals::*` paths so the consumer
/// crate only needs `aether-component` in its dependency list; this
/// alias forwards into the SDK module that owns the symbols.
///
/// Not part of the public API; the macro is the only intended caller.
#[doc(hidden)]
pub mod __macro_internals {
    pub use aether_actor::__macro_internals::*;
}

/// Re-export the [`Actor`] super-trait so component crates pick it up
/// from `aether_component::Actor` without a separate `aether-actor`
/// dependency entry. Issue 525 Phase 3 introduced `Actor`; Phase 4
/// folds `WasmActor: Actor` into the wasm-side surface.
pub use aether_actor::Actor;

/// User-implemented WASM component. ADR-0014 commits to `Self`-is-state —
/// cached kind ids, cached sinks, and any domain fields live on the
/// implementor. `init` runs once before any `receive`; receive is
/// driven by the synthesised `__aether_dispatch` from `#[actor]`.
///
/// Renamed from `Component` to `WasmActor` in issue 525 Phase 4 — the
/// wasm-side leaf of the unified [`Actor`] trait, mirroring the
/// substrate-side `aether_substrate::NativeActor`. The [`Actor`]
/// super-trait owns the symmetric bits (`NAMESPACE`,
/// `FRAME_BARRIER`); `WasmActor` adds the wasm lifecycle methods
/// (`init`, `on_drop`). Hot-swap hooks (`on_replace`,
/// `on_rehydrate`) moved to the opt-in [`Replaceable`] sub-trait so
/// component authors who don't participate in hot-swap aren't asked
/// to know the hooks exist.
///
/// `Component` stays as a backwards-compatibility alias so the
/// `impl Component for X` shape that pre-Phase-4 components write
/// keeps resolving while consumers migrate.
///
/// The `#[no_mangle]` `init` / `receive` exports that actually cross
/// the WASM FFI are generated by `export!(MyComponent)`; implementors
/// do not write `extern "C"` by hand. The trait stays specialised to
/// `WasmTransport` via the `Ctx` / `InitCtx` / `DropCtx` aliases.
pub trait WasmActor: Actor {
    /// Runs once. Resolve kinds and sinks via `ctx` and return the
    /// initial component state. ADR-0033: `#[actor]` prepends
    /// `ctx.subscribe_input::<K>()` for every `K::IS_INPUT` handler
    /// kind so the user body never needs to do it by hand.
    ///
    /// Returns `Result<Self, BootError>` so a component that hits an
    /// unrecoverable startup condition (config parse failure, required
    /// handle missing, malformed env var) can surface its own message
    /// in `LoadResult::Err { error }` instead of the panic-hook path's
    /// generic "guest trapped during init" text. Issue 525 Phase 4b /
    /// issue 531. Most components have no meaningful failure mode and
    /// return `Ok(Self { ... })` unconditionally. A failed `resolve`
    /// inside `init` still panics — see ADR-0012 §2 ("loud at init").
    fn init(ctx: &mut InitCtx<'_>) -> Result<Self, BootError>;

    /// Called once on the instance being dropped — both for
    /// `drop_component` and for the old instance of
    /// `replace_component` — immediately before the substrate tears
    /// down linear memory. Default is no-op; override for cleanup
    /// (sending "goodbye" mail, flushing work to a sibling component,
    /// logging).
    fn on_drop(&mut self, ctx: &mut DropCtx<'_>) {
        let _ = ctx;
    }
}

/// Backwards-compatibility alias for [`WasmActor`]. Lets
/// `impl Component for X` continue to compile during the transition;
/// new code should prefer `WasmActor` directly. Issue 525 Phase 4.
pub use WasmActor as Component;

/// Opt-in trait for components that participate in hot-swap (ADR-0040
/// `replace_component`). Components that don't impl `Replaceable`
/// behave as if both methods were no-ops — the FFI shim emitted by
/// the default `export!()` macro returns `0` for `on_replace` /
/// `on_rehydrate` without dispatching into the trait. To wire the
/// hooks, declare `impl Replaceable for X` and emit via
/// `aether_component::export!(X, replaceable)`.
///
/// Issue 525 Phase 4 split: pre-Phase-4 every `Component` impl
/// inherited default-noop `on_replace` / `on_rehydrate`, so
/// reading the trait surface didn't say which components actually
/// participated in hot-swap. Now the impl list is the source of
/// truth — `impl Replaceable for X` declares "this component
/// survives a `replace_component` call."
pub trait Replaceable: WasmActor {
    /// Called once on the old instance, immediately before a
    /// `replace_component` swap (ADR-0015 §3). Default is no-op;
    /// override to serialize state that the new instance can consume
    /// through `on_rehydrate`, or to emit farewell mail. Prefer
    /// `DropCtx::save_state_kind::<K>` (ADR-0040) to let the kind
    /// system carry schema identity; reach for the raw
    /// `DropCtx::save_state` only when persisting a non-kind blob or
    /// driving an explicit migration off the leading id.
    fn on_replace(&mut self, ctx: &mut DropCtx<'_>) {
        let _ = ctx;
    }

    /// Called after `init` on a freshly-instantiated component that
    /// is replacing an older instance, if and only if the predecessor
    /// produced a state bundle via `DropCtx::save_state` (ADR-0016 §3)
    /// or `DropCtx::save_state_kind` (ADR-0040). Default ignores the
    /// prior state; components that persist across replace override to
    /// rehydrate — typically `prior.as_kind::<MyState>()` for kind-
    /// typed saves, or `prior.bytes()` + `prior.schema_version()` for
    /// the raw path.
    fn on_rehydrate(&mut self, ctx: &mut Ctx<'_>, prior: PriorState<'_>) {
        let _ = ctx;
        let _ = prior;
    }
}

/// Bind a `Component` implementor to the guest's `#[no_mangle]`
/// `init` / `receive` exports. Expands to:
///
/// - A `static` `Slot<T>` that backs the component instance.
/// - `extern "C" fn init(mailbox_id: u64) -> u32` — builds an
///   `InitCtx`, calls `T::init`, stashes the result in the slot.
/// - `extern "C" fn receive(kind, ptr, byte_len, count, sender) -> u32`
///   — builds `Ctx` and `Mail`, calls the `#[actor]`-synthesized
///   `__aether_dispatch` on the stashed instance.
/// - `#[link_section = "aether.kinds.inputs"]` static that pins the
///   component's handler manifest into the wasm custom section the
///   substrate reads at `load_component`. The manifest *bytes* are
///   emitted as associated consts on `T`'s inherent impl by
///   `#[actor]`; this macro is the only place they get a
///   `link_section` attribute, which means the section can only land
///   in the cdylib root that calls `export!()` — never in transitive
///   rlib pulls of a `#[actor]`-using crate (issue 442).
/// - `#[link_section = "aether.namespace"]` static that pins the
///   component's `Component::NAMESPACE` bytes (issue 525 Phase 1B).
///   The substrate reads this at `load_component` and uses it as the
///   default mailbox name when the load payload omits an explicit
///   `name`. Same one-place-per-cdylib invariant as the inputs
///   section — emit only here, never in transitive rlib pulls.
///
/// Only one component per guest crate. A second `export!` call in
/// the same crate is a duplicate-symbol compile error on the shared
/// `init` / `receive` names — ADR-0014 §4 parks multi-component
/// crates as out of scope.
///
/// ```ignore
/// pub struct Hello { /* fields */ }
/// impl aether_component::Component for Hello { /* init + receive */ }
/// aether_component::export!(Hello);
/// ```
///
/// Components that participate in hot-swap (ADR-0040) opt in via the
/// `replaceable` flag — `aether_component::export!(Hello,
/// replaceable);` — which wires the FFI exports through the
/// component's [`Replaceable`] impl. Without the flag, `on_replace`
/// and `on_rehydrate` are emitted as no-ops; the substrate still
/// calls them but the bodies do nothing. Issue 525 Phase 4 made this
/// opt-in explicit so the trait surface tells you which components
/// actually persist across `replace_component`.
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

        // ADR-0033 / issue 442: pin the component's `aether.kinds.inputs`
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

        // Issue 525 Phase 1B: pin the component's `Component::NAMESPACE`
        // bytes into a sibling `aether.namespace` custom section. The
        // substrate reads this at load time as the default mailbox
        // name when the load payload omits an explicit `name`. Length
        // is taken via `len()` so the const-array type is known at
        // compile time without a per-component associated const.
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
        /// Receives the component's own mailbox id (ADR-0030 Phase 2)
        /// so `#[actor]`'s synthesized `init` prologue can self-
        /// address `subscribe_input` for every `K::IS_INPUT` handler
        /// kind. ADR-0060: also installs `MailSubscriber` as the global
        /// `tracing` default before user `init` runs, so logging from
        /// inside `init` reaches the substrate's `aether.log` sink.
        ///
        /// Returns `0` on success and non-zero when the component's
        /// `init` returned `Err(BootError)`. On the `Err` path the
        /// shim ships the error text to the substrate via the
        /// `init_failed_p32` host fn before returning, so the
        /// substrate surfaces the message in `LoadResult::Err`
        /// instead of a generic instantiation error. Issue 525
        /// Phase 4b / issue 531.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn init(mailbox_id: u64) -> u32 {
            $crate::log::install_global_default();
            let mut ctx: $crate::InitCtx<'_> =
                $crate::InitCtx::__new(&$crate::WASM_TRANSPORT, mailbox_id);
            match <$component as $crate::Component>::init(&mut ctx) {
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
                        $crate::raw::init_failed(
                            bytes.as_ptr().addr() as u32,
                            bytes.len() as u32,
                        );
                    }
                    1
                }
            }
        }

        /// # Safety
        /// Called by the substrate with `(kind, ptr, byte_len, count,
        /// sender)` matching the FFI contract in
        /// `aether-substrate/src/host_fns.rs`. `byte_len` is the
        /// total payload size the substrate wrote at `ptr`
        /// (sourced from `mail.payload.len()`); cast decoders sanity-
        /// check it, postcard decoders use it as the exact slice
        /// length. `sender` is the per-instance reply-to handle
        /// (ADR-0013) or `NO_REPLY_HANDLE` for mail with no
        /// meaningful reply target. Exported under the `_p32` suffix
        /// per ADR-0024 Phase 1. Returns the `u32` the
        /// `#[actor]`-synthesized `__aether_dispatch` produces —
        /// `DISPATCH_HANDLED` on match, `DISPATCH_UNKNOWN_KIND` on a
        /// strict-receiver miss (ADR-0033 §Strict receivers).
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
            let mut ctx: $crate::Ctx<'_> = $crate::Ctx::__new(&$crate::WASM_TRANSPORT);
            let mail = unsafe { $crate::Mail::__from_raw(kind, ptr, byte_len, count, sender) };
            instance.__aether_dispatch(&mut ctx, mail)
        }

        /// # Safety
        /// Called by the substrate exactly once, on the old instance,
        /// immediately before a `replace_component` swap. Issue 525
        /// Phase 4: dispatches into the component's
        /// [`Replaceable::on_replace`] when `export!()` was called
        /// with the `replaceable` flag; otherwise the body is a
        /// no-op (the substrate still invokes it for ABI stability).
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn on_replace() -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx: $crate::DropCtx<'_> = $crate::DropCtx::__new(&$crate::WASM_TRANSPORT);
            $crate::__export_internal!(@on_replace $replaceable, $component, instance, ctx);
            0
        }

        /// # Safety
        /// Called by the substrate exactly once on the instance being
        /// dropped, immediately before linear memory is torn down.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn on_drop() -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx: $crate::DropCtx<'_> = $crate::DropCtx::__new(&$crate::WASM_TRANSPORT);
            <$component as $crate::WasmActor>::on_drop(instance, &mut ctx);
            0
        }

        /// # Safety
        /// Called by the substrate after `init` on a freshly
        /// instantiated replacement, with `(version, ptr, len)`
        /// describing the prior-state bundle the old instance
        /// produced via `DropCtx::save_state`. Exported under the
        /// `_p32` suffix per ADR-0024 Phase 1. Same opt-in shape as
        /// `on_replace`: dispatches into [`Replaceable::on_rehydrate`]
        /// when `replaceable` was passed; no-op otherwise.
        #[unsafe(export_name = "on_rehydrate_p32")]
        pub unsafe extern "C" fn on_rehydrate(version: u32, ptr: u32, len: u32) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx: $crate::Ctx<'_> = $crate::Ctx::__new(&$crate::WASM_TRANSPORT);
            let prior = unsafe { $crate::PriorState::__from_raw(version, ptr, len) };
            $crate::__export_internal!(@on_rehydrate $replaceable, $component, instance, ctx, prior);
            0
        }
    };

    // Internal: per-replaceable-flag dispatch arms for the FFI
    // shims. The default `no_replaceable` arm leaves the FFI bodies
    // empty (substrate still calls but no work runs); the
    // `replaceable` arm dispatches into the component's
    // `Replaceable` impl.
    (@on_replace no_replaceable, $component:ty, $instance:ident, $ctx:ident) => {
        // No-op: component opted out of hot-swap.
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

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;

    // Mail / Sink / KindId / PriorState / DropCtx unit coverage moved
    // to `aether-actor` along with the types they exercise. The tests
    // that stay here cover surfaces unique to this crate:
    //   - `Kind::encode_into_bytes` round-trip (issue #240) — the
    //     derive lives in `aether-data`, but the smoke test sits
    //     here because `aether-component`'s `Sink::send` /
    //     `Ctx::send` are the consumer.
    //   - The wasm-aliased `Sink<K>` resolves to `Sink<K, WasmTransport>`
    //     via the type alias — a regression guard against an
    //     accidental alias break.

    #[derive(
        aether_data::Kind,
        aether_data::Schema,
        serde::Serialize,
        serde::Deserialize,
        Debug,
        Clone,
        PartialEq,
    )]
    #[kind(name = "test.fake_postcard")]
    struct FakePostcard {
        tag: alloc::string::String,
        ids: alloc::vec::Vec<u32>,
    }

    #[test]
    fn kind_encode_into_bytes_postcard_roundtrip() {
        let value = FakePostcard {
            tag: alloc::string::String::from("hello"),
            ids: alloc::vec![10, 20, 30],
        };
        let bytes = value.encode_into_bytes();
        // Wire-shape contract: postcard encode matches what the
        // pre-#240 `Sink::send_postcard` path would have written.
        assert_eq!(bytes, postcard::to_allocvec(&value).unwrap());
        let decoded = FakePostcard::decode_from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, value);
    }

    #[test]
    fn kind_encode_into_bytes_cast_roundtrip() {
        #[repr(C)]
        #[derive(
            Copy,
            Clone,
            Debug,
            PartialEq,
            bytemuck::Pod,
            bytemuck::Zeroable,
            aether_data::Kind,
            aether_data::Schema,
        )]
        #[kind(name = "test.encode_cast")]
        struct EncodeCast {
            a: u32,
            b: u32,
        }

        let value = EncodeCast { a: 11, b: 22 };
        let bytes = value.encode_into_bytes();
        // Wire-shape contract: cast encode matches `bytemuck::bytes_of`
        // — what the pre-#240 `Sink::send` path wrote zero-copy.
        assert_eq!(bytes.as_slice(), bytemuck::bytes_of(&value));
        let decoded = EncodeCast::decode_from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, value);
    }

    /// Regression guard for the 1-arg `Sink<K>` alias. If the alias
    /// breaks (e.g. someone parameterises it with a different default
    /// transport) this test fails to compile.
    #[test]
    fn mailbox_alias_resolves_to_wasm_transport() {
        use core::any::TypeId;
        // Building a `Mailbox<FakePostcard>` value via the const
        // resolver is enough — its type identity is what matters
        // here, not the mailbox lookup.
        let _: Mailbox<FakePostcard> = resolve_mailbox::<FakePostcard>("test.smoke");
        assert_eq!(
            TypeId::of::<Mailbox<FakePostcard>>(),
            TypeId::of::<aether_actor::Mailbox<FakePostcard, WasmTransport>>(),
        );
    }
}
