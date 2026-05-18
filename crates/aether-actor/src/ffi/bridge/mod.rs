//! Per-concern FFI bridge ZSTs — the host-fn-facing layer underneath
//! the per-stage capability traits.
//!
//! Issue 665 split the prior monolithic `MailTransport` trait + its
//! `FfiTransport` ZST impl into one ZST per FFI op family:
//!
//! - [`MailBridge`] — outbound mail (`send_mail`, `reply_mail`,
//!   `prev_correlation`). Correlation lives here because every send
//!   mints one regardless of whether the caller sync-waits or
//!   async-handles the reply — it's mail-level metadata, not a
//!   sync-wait artifact.
//! - [`PersistBridge`] — migration-bundle deposit
//!   (`save_state`), used during `on_replace` only.
//! - [`SyncWaitBridge`] — blocking reply receive
//!   (`wait_reply`), the ADR-0042 sync round-trip path.
//!
//! Each ZST has a process-wide `static` instance (`MAIL_BRIDGE`,
//! `PERSIST_BRIDGE`, `SYNC_WAIT_BRIDGE`) so callers borrow
//! `&MAIL_BRIDGE` etc. without instantiating per-call. The `_BRIDGE`
//! suffix is intentional — `aether_actor::Mail<'_>` (the inbound
//! envelope decoder) is a different type at a different path, and
//! the suffix keeps the unqualified names disambiguated. The methods
//! are inherent — there is no shared trait across the three because
//! their op families don't overlap.
//!
//! Per-stage capability ctx impls in [`crate::ffi::ctx`] route through
//! these statics directly; the cross-target abstraction layer is the
//! per-stage capability traits in [`crate::actor::ctx`], not a single
//! transport trait.

pub mod mail;
pub mod persist;
pub mod sync_wait;

pub use mail::{MAIL_BRIDGE, MailBridge};
pub use persist::{PERSIST_BRIDGE, PersistBridge};
pub use sync_wait::{SYNC_WAIT_BRIDGE, SyncWaitBridge};
