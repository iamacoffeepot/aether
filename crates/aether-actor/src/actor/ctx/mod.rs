//! Per-stage capability traits + the parametric ctx structs that back
//! the FFI guest path today.
//!
//! Issue 663 phase A factors the ctx surface into per-stage trait
//! files — each describes a slice of functionality applicable at one
//! lifecycle stage:
//!
//! - [`MailSender`] — outbound mail (every ctx).
//! - [`OutboundReply`] — reply-to-originator (per-handler ctxs only).
//! - [`Resolver`] — init-time mailbox/kind resolution (init ctxs only).
//! - [`Persistence`] — `replace_component` migration bundle (drop ctxs only).
//! - [`LifecycleControl`] — self-shutdown + monitor (per-handler ctxs that
//!   participate in ADR-0079 lifecycle).
//!
//! The parametric [`Ctx`] / [`InitCtx`] / [`DropCtx`] structs in
//! [`parametric`] back the FFI guest path today; phase C concretises
//! them into [`crate::wasm::WasmCtx`] / etc. (renamed to `FfiCtx` in
//! the same phase) and retires the parametric core. The trait surface
//! defined here will remain the user-facing contract.

pub mod lifecycle;
pub mod mail_sender;
pub mod outbound_reply;
pub mod parametric;
pub mod persistence;
pub mod resolver;

pub use lifecycle::LifecycleControl;
pub use mail_sender::MailSender;
pub use outbound_reply::OutboundReply;
pub use parametric::{Ctx, DropCtx, InitCtx};
pub use persistence::Persistence;
pub use resolver::Resolver;
