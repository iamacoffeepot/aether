//! [`PersistBridge`] — migration-bundle FFI bridge.
//!
//! ZST whose only inherent method is `save_state`, the ADR-0016
//! deposit hook called inside `on_replace` to hand a typed bundle to
//! the replacement instance. PersistBridgeence is conceptually distinct
//! from mail (it's a one-shot byte deposit, not a routed envelope),
//! so it lives in its own bridge rather than on [`super::mail::MailBridge`].
//!
//! Native actors do not have a `replace_component`-style hot reload
//! path — only wasm components do. The native ctx structs deliberately
//! do not impl [`crate::actor::ctx::PersistBridgeence`].

use crate::ffi::raw;

/// ZST FFI bridge for `save_state`. Borrow [`PERSIST_BRIDGE`] from any ctx
/// that impls [`crate::actor::ctx::PersistBridgeence`] to forward the
/// typed-bundle bytes through the host fn.
pub struct PersistBridge;

/// Process-wide [`PersistBridge`] instance.
pub static PERSIST_BRIDGE: PersistBridge = PersistBridge;

impl PersistBridge {
    /// Deposit a migration bundle for the substrate to hand to the
    /// replacement instance via `on_rehydrate` (ADR-0016). Bytes are
    /// copied into a substrate-owned buffer immediately, so the
    /// caller is free to drop the slice on return.
    ///
    /// Returns `0` on success; non-zero on substrate rejection
    /// (today: 1 MiB cap exceeded or internal OOB — both component
    /// bugs). Only meaningful inside `on_replace`; calling from
    /// `on_drop` is technically accepted by the host fn, but the
    /// bytes are then discarded (ADR-0016 §5 — plain drops have no
    /// successor).
    pub fn save_state(&self, version: u32, bytes: &[u8]) -> u32 {
        unsafe { raw::save_state(version, bytes.as_ptr().addr() as u32, bytes.len() as u32) }
    }
}
