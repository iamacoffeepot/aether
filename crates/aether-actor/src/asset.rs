//! ADR-0250 asset vocabulary — the ctx-trait surface an actor uses to read
//! the assets it ships in `aether.asset.<path>` wasm custom sections.
//!
//! One trait covers "what does this bundle carry" and "give me the payload",
//! and it works in every hook: `init`, `wire`, every handler, `on_rehydrate`,
//! and `unwire`. It is implemented by every wasm ctx (`WasmInitCtx`, `WireCtx`,
//! and `WasmCtx`, which covers every handler, `on_rehydrate`, and `unwire`).
//! The host serves each read from the instance's own module, whose publish
//! checked each asset in as its own deduplicated blob, so a spawn of a
//! published type always builds an instance that can read its assets.
//!
//! # Two verbs, by what the actor does with the payload
//!
//! - [`Assets::asset_blob`] hands the asset over as a [`Blob`] the guest
//!   holds by handle. No payload byte enters guest memory: the blob is the
//!   asset's own store entry. Take it when the actor **routes** the asset,
//!   mailing it on as a `Blob` field to the actor that consumes it (a texture
//!   upload, a mesh load): the recipient reads the same bytes in place.
//! - [`Assets::asset`] copies the asset into guest memory as a `Vec<u8>`.
//!   Take it when the actor **reads** the payload itself: parses it, slices
//!   it, or transforms it before anything leaves. A blob can be read too,
//!   through [`aether_data::BlobReader`], a range at a time; `asset` is the
//!   plain form when the whole payload is wanted in hand.
//!
//! The catalog is indexed host-side from the custom sections without
//! instantiating the component (`aether-substrate`'s asset section indexer).
//! A name the catalog does not carry answers `None`, as payload access always
//! has.

use aether_data::Blob;
use alloc::vec::Vec;

pub use aether_kinds::AssetInfo;

/// Payload access to a component's assets, live in every hook (ADR-0250).
/// Implemented by every wasm ctx, so an asset read from a handler,
/// `on_rehydrate`, or `unwire` serves the instance's own module exactly as a
/// read from `init` or `wire` does. Both verbs match `name` (the
/// `aether.asset.` section suffix) against the catalog exactly.
pub trait Assets {
    /// The catalog entries, in the order the sections were indexed. Empty
    /// for a component that carries no assets.
    fn assets(&self) -> &[AssetInfo];

    /// The bytes of the asset named `name`, copied into guest memory, or
    /// `None` when the component carries no such asset. The bytes are the
    /// actor's to keep or drop.
    fn asset(&mut self, name: &str) -> Option<Vec<u8>>;

    /// The asset named `name` as a blob the guest holds by handle, or `None`
    /// when the component carries no such asset. No byte enters guest memory
    /// until the blob is read: sent on as a `Blob` field, it reaches an
    /// in-process recipient as the same bytes, uncopied. It stays resident
    /// until its last clone drops.
    fn asset_blob(&mut self, name: &str) -> Option<Blob>;
}
