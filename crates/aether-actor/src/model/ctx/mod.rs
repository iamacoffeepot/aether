//! Per-stage capability traits — the cross-transport ctx contract.
//!
//! Each trait describes a slice of functionality applicable at one
//! lifecycle stage:
//!
//! - [`MailSender`] — outbound mail (every ctx).
//! - [`OutboundReply`] — reply-to-originator (per-handler ctxs only).
//! - [`Emit`] — multi-class 0..n emission (multi-mode ctxs only, ADR-0134).
//! - [`Persistence`] — `replace_component` migration bundle (drop
//!   ctxs only).
//!
//! The concrete ctx structs live next to their transport: FFI-side
//! `WasmInitCtx` / `WasmCtx` / `WasmDropCtx` in [`crate::wasm::ctx`];
//! native-side `NativeInitCtx` / `NativeCtx` in
//! `aether_substrate::actor::native::ctx`. Each impls the trait
//! subset applicable to its stage; default-impl bodies on
//! [`MailSender`] cover the routing methods so the per-impl code is
//! the stage-specific accessors.

pub mod emit;
pub mod mail_sender;
pub mod outbound_reply;
pub mod persistence;
pub mod reply_mode;

pub use emit::Emit;
pub use mail_sender::MailSender;
pub use outbound_reply::OutboundReply;
pub use persistence::Persistence;
pub use reply_mode::{Manual, Multi, ReplyMode, Single};

/// The actor marker of a ctx that names no actor.
///
/// A ctx that names its actor reaches the calls that are sound only for the
/// actor being dispatched — natively, `spawn_child` lives only on the typed
/// form, so the parent of a staged birth is read off the ctx rather than
/// declared beside it, and a caller has no way to name a parent the runtime
/// will then contradict (issue 4158). Every ctx built where no actor is in
/// scope — a guest entry point, `wire` / `unwire`, the chassis root, a test
/// fixture — is this form, and loses only a call it could not have made
/// correctly.
///
/// A type-position marker like [`Single`] / [`Manual`], never a value: it is
/// only ever the `A` of a [`WasmCtx`](crate::WasmCtx) / `NativeCtx`, so it
/// carries no impls of its own.
pub struct Erased;
