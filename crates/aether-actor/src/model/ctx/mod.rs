//! Per-stage capability traits — the cross-transport ctx contract.
//!
//! Each trait describes a slice of functionality applicable at one
//! lifecycle stage:
//!
//! - [`MailSender`] — outbound mail (every ctx).
//! - [`OutboundReply`] — reply-to-originator (per-handler ctxs only).
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

pub mod mail_sender;
pub mod outbound_reply;
pub mod persistence;
pub mod reply_mode;

pub use mail_sender::MailSender;
pub use outbound_reply::OutboundReply;
pub use persistence::Persistence;
pub use reply_mode::{Manual, ReplyMode, Single, Stream};
