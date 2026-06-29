//! Per-concern FFI bridge modules — the host-fn-facing layer underneath
//! the per-stage capability traits.
//!
//! Issue 665 split the prior monolithic `MailTransport` trait into one
//! module per FFI op family. Issue 1967 then collapsed the per-module
//! ZST + static packaging into free functions,
//! keeping the safe-wrapper boundary (one `unsafe` block per FFI op, one
//! audited ptr/len marshalling) while closing the over-exposure the
//! `pub static` forms created.
//!
//! - `log` — log-event FFI (`emit_log_event`). Split from `mail` because
//!   it is a distinct op family with no relation to mail routing.
//! - `mail` — outbound mail (`send_mail`, `reply_mail`,
//!   `prev_correlation`, `spawn_sibling`, `spawn_inline_child`).
//!   Correlation lives here because every send mints one so a handler
//!   can match a reply to the request it sent — it's mail-level metadata.
//! - `persist` — migration-bundle deposit
//!   (`save_state`), used during `on_dehydrate` only.
//!
//! Per-stage capability ctx impls in [`crate::wasm::ctx`] call these
//! functions directly; the cross-target abstraction layer is the
//! per-stage capability traits in [`crate::model::ctx`], not a single
//! transport trait.

pub(crate) mod log;
pub(crate) mod mail;
pub(crate) mod persist;
