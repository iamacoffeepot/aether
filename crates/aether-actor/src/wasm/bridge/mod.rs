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
//!   `prev_correlation`, `spawn_inline_child`).
//!   Correlation lives here because every send mints one so a handler
//!   can match a reply to the request it sent — it's mail-level metadata.
//! - `persist` — migration-bundle deposit
//!   (`save_state`), used during `on_dehydrate` only.
//! - `asset` — ADR-0163 load-window asset pull (`asset_fetch`,
//!   `asset_catalog`), used during `init` / `wire` only.
//! - `blob` — ADR-0238 guest blob reads (`blob_hold`, `blob_read`,
//!   `blob_drop`), the transport under the guest's `GuestHold` backing.
//!   wasm32-only: every caller is.
//! - `init` — ADR-0096 guest init-failure staging (`init_failed`), the
//!   transport the `export!` init shims call before returning non-zero.
//!   wasm32-only: every caller is.
//! - `address` — ADR-0230 §3 path proof (`resolve_path`), the transport under
//!   `WasmCtx::resolve_path`, and the `__ResolvedPath` answer it decodes;
//!   ADR-0231 §4 published rows (`published_rows`), the transport under
//!   `WasmCtx::cast`, and the `__PublishedRows` answer it decodes; and
//!   ADR-0231 §3 route rows (`route_rows`), the transport under a guest's
//!   decode of a `ProtocolPath`, which answers a `__PublishedRows` too.
//!
//! Per-stage capability ctx impls in [`crate::wasm::ctx`] call these
//! functions directly; the cross-target abstraction layer is the
//! per-stage capability traits in [`crate::model::ctx`], not a single
//! transport trait.

pub(crate) mod address;
pub(crate) mod asset;
#[cfg(target_arch = "wasm32")]
pub(crate) mod blob;
#[cfg(target_arch = "wasm32")]
pub(crate) mod init;
pub(crate) mod log;
pub(crate) mod mail;
pub(crate) mod persist;

/// A guest address or length as the `_p32` ABI's `u32`. Guest memory is
/// addressed by 32 bits on wasm32, so the conversion is exact there;
/// saturating keeps it total without a cast.
fn abi32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
