//! ADR-0163 §3 asset load-window vocabulary — the ctx-trait surface an
//! actor uses to read the assets it ships in `aether.asset.<path>` wasm
//! custom sections.
//!
//! Two traits split "what does this bundle carry" (available for the
//! instance's life) from "give me the payload" (available only during the
//! load window — `init` + `wire`):
//!
//! - [`AssetCatalog`] lists the [`AssetInfo`] entries — name and
//!   length. A few hundred bytes of metadata, so it stays queryable for
//!   the instance's life and surfaces through `describe_component`.
//! - [`AssetWindow`] adds payload access. It is implemented only by the
//!   load-window ctxs (`init` / `wire`), so a "fetch later" from a
//!   handler is a compile error rather than a runtime surprise: when
//!   `wire` returns, the host lets go of the module's bytes and the
//!   payload path is gone (ADR-0163 §3/§4).
//!
//! # Two verbs, by what the actor does with the payload
//!
//! - [`AssetWindow::asset_blob`] hands the asset over as a [`Blob`] the
//!   guest holds by handle. No payload byte enters guest memory: the blob
//!   is a range of the module bytes where they already sit in the engine's
//!   store. Take it when the actor **routes** the asset, mailing it on as a
//!   `Blob` field to the actor that consumes it (a texture upload, a mesh
//!   load): the recipient reads the same bytes in place. It fits a module
//!   that is mostly payload, since whoever keeps the blob keeps the whole
//!   module file resident with it.
//! - [`AssetWindow::asset`] copies the asset into guest memory as a
//!   `Vec<u8>`. Take it when the actor **reads** the payload itself: parses
//!   it, slices it, or transforms it before anything leaves. A blob can be
//!   read too, through [`aether_data::BlobReader`], a range at a time;
//!   `asset` is the plain form when the whole payload is wanted in hand, and
//!   the right one for a small asset in a module that is mostly code: the
//!   copy costs the asset's size, where a kept blob would hold the module.
//!
//! A blob the actor keeps past `wire` in its own state stays resident, and
//! holds the whole module file it is a range of resident with it, until the
//! actor drops it or its instance ends. That is the actor's choice and it is
//! visible in the store's resident count; the window itself still lets go of
//! the module bytes when `wire` returns.
//!
//! The catalog is indexed host-side from the custom sections without
//! instantiating the component (`aether-substrate`'s asset section
//! indexer, #3969). Payloads are read from the recorded range in
//! the module bytes the load, spawn, or republish brought, for the duration
//! of the window. A spawn brings them in its `code` field, as a boot
//! manifest entry's spawns do. An instance spawned without them fetches no
//! payload: a catalogued asset traps by either verb, naming the two doors
//! that bring the bytes, a spawn with its code and a load (ADR-0163 §4).

use aether_data::Blob;
use alloc::vec::Vec;

pub use aether_kinds::AssetInfo;

/// The asset catalog of a loaded component — one [`AssetInfo`] per
/// `aether.asset.<path>` custom section it carries (ADR-0163 §3). Metadata
/// only (name / length), so it is cheap to keep for the
/// instance's life; implemented by every load-window ctx and by the
/// host-side served window. Payload access is the separate
/// [`AssetWindow`].
pub trait AssetCatalog {
    /// The catalog entries, in the order the sections were indexed. Empty
    /// for a component that carries no assets.
    fn assets(&self) -> &[AssetInfo];
}

/// Payload access to a component's assets, live only during the load
/// window — `init` plus `wire` (ADR-0163 §3). Implemented by the
/// window-bearing ctxs alone, so a post-window fetch does not typecheck.
/// Both verbs read the recorded byte range of the module bytes the load
/// brought, and match `name` (the `aether.asset.` section suffix) against
/// the catalog exactly; the module docs say when each fits. When the window
/// closes the host lets go of those bytes, so a later call (were one
/// reachable) traps.
pub trait AssetWindow: AssetCatalog {
    /// The bytes of the asset named `name`, copied into guest memory, or
    /// `None` when the component carries no such asset. The bytes are the
    /// actor's to keep or drop.
    fn asset(&mut self, name: &str) -> Option<Vec<u8>>;

    /// The asset named `name` as a blob the guest holds by handle, or `None`
    /// when the component carries no such asset. No byte enters guest memory
    /// until the blob is read: sent on as a `Blob` field, it reaches an
    /// in-process recipient as the same bytes, uncopied. It stays resident,
    /// with the module bytes it is a range of, until its last clone drops.
    fn asset_blob(&mut self, name: &str) -> Option<Blob>;
}
