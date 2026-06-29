//! [`Persistence`] — `replace_component` migration-bundle deposit.
//!
//! Per-stage capability trait under the issue 663 refactor. The
//! `on_dehydrate` ctx impls this; runtime and init ctxs deliberately
//! do not (init has no prior bundle to consume; runtime saves are
//! deferred to the `on_dehydrate` hook so the substrate can hand the
//! bundle to the replacement instance).
//!
//! `save_state` is meaningful in `on_dehydrate`, the save-side
//! hot-swap hook the substrate runs immediately before a
//! `replace_component` splice.
//!
//! `save_state_kind` is the typed-state convenience (ADR-0040): the
//! bundle is framed as `[0..8)` little-endian `K::ID` followed by the
//! wire encoding of `value`; the replacement instance recovers `K`
//! via [`crate::mail::PriorState::decode_kind`]. Use the raw `save_state`
//! when persisting bytes that aren't a kind or when driving an
//! explicit migration off the leading id.

use alloc::vec::Vec;

use aether_data::{Kind, Schema, wire};

/// Migration-bundle deposit surface for the `on_dehydrate` save hook.
/// Init / runtime ctxs deliberately don't implement this.
///
/// The trait has no associated types — `save_state` is a pure byte
/// deposit and the host fn signature is identical across transports.
pub trait Persistence {
    /// Deposit a migration bundle for the substrate to hand to the
    /// replacement instance via `on_rehydrate`. `version` is
    /// component-defined (the substrate doesn't interpret it); bytes
    /// are copied into a substrate-owned buffer immediately, so the
    /// caller is free to drop the slice on return.
    ///
    /// Panics if the substrate rejects the call — today that's only
    /// the 1 MiB cap being exceeded or an internal OOB, both of
    /// which are component bugs. ADR-0015's trap containment ensures
    /// the panic doesn't stall teardown on the substrate side.
    ///
    /// May be called zero or one times per `on_dehydrate`; a second
    /// call overwrites.
    fn save_state(&mut self, version: u32, bytes: &[u8]);

    /// Persist a typed kind value across `replace_component`
    /// (ADR-0040). The bundle is framed as `[0..8)` little-endian
    /// `K::ID` followed by the wire encoding of `value`; the
    /// replacement instance recovers `K` via [`crate::mail::PriorState::decode_kind`].
    ///
    /// `K::ID` is the ADR-0030 schema hash — changing the shape of
    /// `K` changes the id, which is what makes `decode_kind::<K>`
    /// automatically reject stale bytes after a schema evolution.
    /// `version` is passed through to the substrate unchanged;
    /// components typically leave it `0` since `K::ID` already
    /// identifies the schema, but a non-zero value is legal for
    /// components that want to stack a migration counter on top of
    /// kind identity.
    fn save_state_kind<K>(&mut self, version: u32, value: &K)
    where
        K: Kind + Schema + serde::Serialize,
    {
        let mut out = Vec::from(K::ID.0.to_le_bytes());
        let payload = wire::to_vec(value).expect("wire encode to Vec is infallible");
        out.extend_from_slice(&payload);
        self.save_state(version, &out);
    }
}
