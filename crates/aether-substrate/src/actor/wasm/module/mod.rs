//! Code as a value (ADR-0241 §2): a [`Module`] is a compiled cache entry made
//! from a [`Blob`] of wasm bytes.
//!
//! Checking code in through the engine's one [`ModuleCache`] derives
//! everything the engine needs from the bytes once per content hash:
//!
//! - the compiled `wasmtime::Module`;
//! - the [`ModuleManifest`], every custom section the host reads (kinds,
//!   exported and private actor groups, lineage, boot, namespace, the
//!   no-default and content-addressed markers, and the asset catalog), parsed
//!   once;
//! - each `aether.asset.*` section, checked into the blob store as its own
//!   [`Blob`].
//!
//! The wasm bytes are used to compile and to parse, and are then let go:
//! nothing here holds the code blob, so the bytes leave the store once the
//! caller drops its value. Every later load, boot and replace of the same
//! bytes reads the entry instead of the bytes.
//!
//! A module publishes the namespaces [`Module::published_groups`] names
//! (ADR-0241 §3): its exported groups' declared namespaces, each qualified by
//! the module's hash when the module is content-addressed.
//!
//! A `Module` has no public constructor; [`ModuleCache::check_in`] is the only
//! way to get one. A clone shares its entry, and the entry lives while any
//! clone does.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use aether_data::{Blob, BlobHash};

use crate::actor::wasm::kind_manifest::ActorInputs;

mod cache;
mod manifest;
#[cfg(test)]
mod tests;

pub use cache::ModuleCache;
pub use manifest::ModuleManifest;

/// The length of a module hash in lowercase hex, as a content-addressed
/// module's published namespaces carry it.
const HASH_HEX_BYTES: usize = 2 * size_of::<BlobHash>();

/// A compiled, parsed module: one content hash's cache entry. Cheap to clone;
/// every clone shares one entry. See the module docs.
#[derive(Clone)]
pub struct Module {
    entry: Arc<ModuleEntry>,
}

/// What a module's bytes are checked in as. Built only by
/// [`ModuleCache::check_in`].
struct ModuleEntry {
    hash: BlobHash,
    compiled: wasmtime::Module,
    manifest: ModuleManifest,
    assets: Arc<[(AssetName, Blob)]>,
}

impl Module {
    /// The ADR-0238 BLAKE3 hash of the wasm bytes this module was made from:
    /// its identity in the engine.
    #[must_use]
    pub fn hash(&self) -> BlobHash {
        self.entry.hash
    }

    /// The compiled code, compiled once per hash per engine.
    #[must_use]
    pub fn compiled(&self) -> &wasmtime::Module {
        &self.entry.compiled
    }

    /// The module's custom sections, parsed once per hash.
    #[must_use]
    pub fn manifest(&self) -> &ModuleManifest {
        &self.entry.manifest
    }

    /// Every exported group under the namespace it publishes (ADR-0241 §3),
    /// in declaration order: its declared namespace, or, for a
    /// content-addressed module, `{namespace}.{hash}` with the module's hash
    /// in 64 lowercase hex, so every build is its own publication. Inside the
    /// module each type keeps its declared namespace: the export selector and
    /// the type tag read [`ModuleManifest::exported_groups`].
    pub fn published_groups(&self) -> impl Iterator<Item = (Cow<'_, str>, &ActorInputs)> {
        let hash = self.manifest().content_addressed().then(|| blake3::Hash::from_bytes(*self.hash().as_bytes()));
        self.manifest().exported_groups().map(move |(namespace, group)| {
            let published =
                hash.map_or(Cow::Borrowed(namespace), |hash| Cow::Owned(format!("{namespace}.{}", hash.to_hex())));
            (published, group)
        })
    }

    /// Each `aether.asset.*` section by asset name, in section order, as its
    /// own blob.
    #[must_use]
    pub fn assets(&self) -> &[(AssetName, Blob)] {
        &self.entry.assets
    }

    /// The asset blobs as one shared handle, for a load window to hold and
    /// then let go of.
    pub(crate) fn shared_assets(&self) -> Arc<[(AssetName, Blob)]> {
        Arc::clone(&self.entry.assets)
    }
}

impl fmt::Debug for Module {
    /// The identity, never the code or the manifest.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Module").field("hash", &self.entry.hash).finish_non_exhaustive()
    }
}

/// An asset's catalog name: its custom section's name after
/// [`ASSET_SECTION_PREFIX`](super::asset_manifest::ASSET_SECTION_PREFIX).
/// Never empty. Only the manifest parse mints one.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AssetName(Box<str>);

impl AssetName {
    /// The name, refused when empty.
    fn new(name: &str) -> Result<Self, String> {
        if name.is_empty() {
            return Err(format!("{}: empty asset path", super::asset_manifest::ASSET_SECTION_PREFIX));
        }
        Ok(Self(name.into()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AssetName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for AssetName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
