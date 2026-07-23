//! ADR-0163 §3 asset load-window vocabulary — the ctx-trait surface an
//! actor uses to read the assets it ships in `aether.asset.<path>` wasm
//! custom sections.
//!
//! Two traits split "what does this bundle carry" (available for the
//! instance's life) from "give me the bytes" (available only during the
//! load window — `init` + `wire`):
//!
//! - [`AssetCatalog`] lists the [`AssetInfo`] entries — name, length,
//!   sha256. A few hundred bytes of metadata, so it stays queryable for
//!   the instance's life and surfaces through `describe_component`.
//! - [`AssetWindow`] adds payload access. It is implemented only by the
//!   load-window ctxs (`init` / `wire`), so a "fetch later" from a
//!   handler is a compile error rather than a runtime surprise: when
//!   `wire` returns, the host drops the asset index and the payload path
//!   is gone (ADR-0163 §3/§4).
//!
//! The catalog is indexed host-side from the custom sections without
//! instantiating the component (`aether-substrate`'s asset section
//! indexer, #3969). Payload bytes are read from the recorded range in
//! the module file for the duration of the window; nothing payload-sized
//! outlives it.

use alloc::vec::Vec;

pub use aether_kinds::AssetInfo;

/// The asset catalog of a loaded component — one [`AssetInfo`] per
/// `aether.asset.<path>` custom section it carries (ADR-0163 §3). Metadata
/// only (name / length / sha256), so it is cheap to keep for the
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
/// [`asset`](AssetWindow::asset) reads the recorded byte range straight
/// from the module file; the returned bytes are the actor's to keep or
/// drop. When the window closes the index is released, so a later call
/// (were one reachable) yields `None`.
pub trait AssetWindow: AssetCatalog {
    /// The bytes of the asset named `name` (the `aether.asset.` section
    /// suffix), or `None` when the component carries no such asset or the
    /// window has closed. The name is matched against the catalog exactly.
    fn asset(&mut self, name: &str) -> Option<Vec<u8>>;
}
