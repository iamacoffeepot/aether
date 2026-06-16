//! Per-concern FFI bridge modules — the host-fn-facing layer underneath
//! the per-stage capability traits.
//!
//! Issue 665 split the prior monolithic `MailTransport` trait + its
//! `FfiTransport` ZST impl into one module per FFI op family. Issue 1967
//! then collapsed the per-module ZST + static packaging into `pub(crate)`
//! free functions, keeping the safe-wrapper boundary (one `unsafe` block
//! per FFI op, one audited ptr/len marshalling) while closing the
//! over-exposure the `pub static` forms created.
//!
//! - [`mail`] — outbound mail (`send_mail`, `reply_mail`,
//!   `prev_correlation`, `source_of`, `emit_log_event`, `spawn_sibling`,
//!   `spawn_inline_child`). Correlation lives here because every send
//!   mints one so a handler can match a reply to the request it sent —
//!   it's mail-level metadata.
//! - [`persist`] — migration-bundle deposit
//!   (`save_state`), used during `on_dehydrate` only.
//!
//! Per-stage capability ctx impls in [`crate::ffi::ctx`] call these
//! functions directly; the cross-target abstraction layer is the
//! per-stage capability traits in [`crate::actor::ctx`], not a single
//! transport trait.

pub(crate) mod mail;
pub(crate) mod persist;
