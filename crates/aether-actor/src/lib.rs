//! aether-actor: target-agnostic actor SDK shared by WASM components
//! (via `aether-component`'s `WasmTransport`) and, eventually, native
//! capabilities (via `aether-substrate`'s `NativeTransport`).
//!
//! ADR-0074 §Decision settles the actor model: components and
//! capabilities collapse into one actor primitive — one mpsc inbox,
//! one OS thread, one `MailboxId` — and share this SDK over two
//! transport implementations. Phase 1 lifts the SDK out of
//! `aether-component` and re-bases the wasm guest path on a
//! `WasmTransport` impl of [`MailTransport`]; Phase 2 adds
//! `NativeTransport` so capabilities migrate off their hand-rolled
//! `Arc<AtomicBool>` shutdown loops onto the same SDK.
//!
//! Public surface:
//!   - [`MailTransport`] — the five-method trait every transport
//!     impl must provide; signatures mirror `aether-component`'s
//!     `_p32` FFI byte-for-byte.
//!   - [`Mail`], [`PriorState`], [`ReplyTo`], [`KindId`] —
//!     transport-free types: pure decode / phantom typing.
//!   - [`Mailbox`], [`Ctx`], [`InitCtx`], [`DropCtx`] — generic over
//!     `T: MailTransport`; method bodies dispatch through `T::*`.
//!   - [`Slot`] — single-instance backing store the consumer's
//!     `export!` macro emits as a `static`.
//!   - [`WaitError`] + [`wait_reply`] + [`decode_wait_reply`] —
//!     ADR-0042 sync round-trip helper, generic over the reply kind,
//!     the error enum, and the transport.
//!   - [`handle`] — typed-handle SDK (ADR-0045) generic over `T`;
//!     `Handle<K, T>::release` / `pin` / `unpin` and the `publish`
//!     helper share one code body across guest and native.
//!
//! No FFI imports, no `extern "C"`, no panic handlers. Those live
//! with the transport-specific shim that wraps the SDK
//! (`aether-component` for the wasm side).

#![no_std]

extern crate alloc;

mod ctx;
pub mod handle;
mod mail;
mod sink;
mod slot;
mod sync;
mod transport;

pub use ctx::{Ctx, DropCtx, InitCtx};
pub use mail::{Mail, NO_REPLY_HANDLE, PriorState, ReplyTo};
pub use sink::{KindId, Mailbox, resolve, resolve_mailbox};
pub use slot::Slot;
pub use sync::{WaitError, decode_wait_reply, wait_reply};
pub use transport::MailTransport;

/// Return code the `#[handlers]`-synthesized dispatcher sends back up
/// through `receive_p32` when a `#[handler]` arm matched (or the
/// `#[fallback]` ran, which by definition handles anything). Propagated
/// verbatim by the consumer's FFI shim.
pub const DISPATCH_HANDLED: u32 = 0;

/// Return code for "no `#[handler]` matched and there's no `#[fallback]`"
/// — the strict-receiver miss. Propagated through the FFI so the
/// substrate's scheduler can emit a `tracing::warn!` naming the
/// mailbox + kind (ADR-0033 §Strict receivers, issue #142). Matches
/// `aether_substrate_bundle::component::DISPATCH_UNKNOWN_KIND` by value.
pub const DISPATCH_UNKNOWN_KIND: u32 = 1;

/// Re-exports the `#[handlers]` macro relies on at expansion sites
/// that don't depend on `aether-data` directly. Keeping the macro's
/// emitted paths rooted at `::aether_component::__macro_internals` (which
/// re-exports this module) removes the "add aether-data to your
/// Cargo.toml" boilerplate that `::aether_data::...` paths would
/// otherwise force on every consumer.
///
/// Not part of the public API; the macro is the only intended caller.
#[doc(hidden)]
pub mod __macro_internals {
    pub use aether_data::__derive_runtime::{Cow, KindLabels, SchemaType, canonical};
    pub use aether_data::{Kind, Schema};
}
