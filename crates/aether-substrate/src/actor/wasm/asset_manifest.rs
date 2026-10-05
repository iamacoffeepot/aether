//! ADR-0163 §3 asset section indexer + the host-served load window
//! (#3969).
//!
//! A component that ships assets declares them with `export_asset!`
//! (ADR-0163 §2, #3979), which lands each asset's bytes in a wasm custom
//! section named `aether.asset.<path>` — the same emission path as the
//! `aether.kinds` sections (see [`super::kind_manifest`]), never
//! instantiated by wasm execution. This module reads those sections
//! host-side before compilation, exactly as `kind_manifest` reads
//! `aether.kinds`: [`read_assets_from_bytes`] walks the raw bytes with
//! `wasmparser` and records, per asset, its catalog entry ([`AssetInfo`]:
//! name, length, sha256) plus the byte range of its payload within the
//! module file. The module cache runs it once per content hash and keeps
//! the catalog and the ranges, never the payloads (ADR-0241 §2,
//! [`super::module`]).
//!
//! The catalog is metadata — a few hundred bytes — so it is kept for the
//! instance's life and surfaces through `describe_component`
//! ([`aether_kinds::ComponentCapabilities::assets`]). Payload access is
//! the [`LoadWindow`]: it holds the code blob its opener brought (the
//! load's, or the republish's) and serves [`LoadWindow::fetch`] by
//! streaming the named asset's range out of it, or
//! [`LoadWindow::fetch_blob`] by viewing that range in place as a blob of
//! its own. [`LoadWindow::close`] lets go of the code blob when the load
//! window ends (`init` + `wire`), so the guest's payload access ends with
//! the window and nothing payload-sized outlives it but an asset blob the
//! guest chose to keep, which holds the code resident until it drops
//! (ADR-0163 §3/§4). An instance spawned from a
//! publication has no code in hand: its window answers the catalog and
//! refuses a catalogued asset, naming `load_component`.
//!
//! This reads the custom sections only and never looks at linear memory.
//! `export_asset!` keeps the payload out of linear memory on its own (it
//! withholds the `#[used]` pin, so the dead `#[link_section]` static is
//! garbage-collected before the shipped wasm; #3981), and this indexer
//! would be correct either way. Do not read a "not in linear memory"
//! property out of this module; it makes no such claim.

use std::ops::Range;

use aether_actor::AssetCatalog;
use aether_data::{Blob, BlobReader};
use aether_kinds::AssetInfo;
use sha2::{Digest, Sha256};
use wasmparser::{Parser, Payload};

use super::module::{AssetSection, Module};
use crate::actor::native::BlobCheckIn;

/// The prefix every asset custom section's name carries (ADR-0163 §2). An
/// asset's catalog name is its section name with this prefix stripped.
pub const ASSET_SECTION_PREFIX: &str = "aether.asset.";

/// One indexed asset: its catalog entry ([`AssetInfo`]) plus the byte
/// range of its payload within the module file. The range is what a load
/// window reads from its opener's code (ADR-0163 §3).
#[derive(Debug, Clone)]
pub struct AssetRecord {
    /// Catalog metadata: name, length, sha256.
    pub info: AssetInfo,
    /// Offset of the asset's payload bytes within the module file.
    pub offset: usize,
    /// Length of the asset's payload bytes (equals `info.len`).
    pub len: usize,
}

/// Walk a module's `aether.asset.*` custom sections and index each asset
/// (ADR-0163 §3): catalog name (the section-name suffix after
/// [`ASSET_SECTION_PREFIX`]), byte length, sha256, and byte range into
/// `wasm`. Sections without the prefix are ignored; a module carrying no
/// asset sections returns an empty vec.
///
/// A section name appearing more than once, or an empty asset path
/// (`aether.asset.` with nothing after it), is a hard error — defense in
/// depth behind `export_asset!`'s link-time duplicate-name guard (#3979),
/// so a hand-assembled or corrupt module fails the load loudly rather
/// than serving an ambiguous or truncated payload.
pub fn read_assets_from_bytes(wasm: &[u8]) -> Result<Vec<AssetRecord>, String> {
    let mut records: Vec<AssetRecord> = Vec::new();

    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|e| format!("wasmparser: {e}"))?;
        let Payload::CustomSection(reader) = payload else {
            continue;
        };
        let Some(path) = reader.name().strip_prefix(ASSET_SECTION_PREFIX) else {
            continue;
        };
        if path.is_empty() {
            return Err(format!("{ASSET_SECTION_PREFIX}: empty asset path (custom section named {:?})", reader.name()));
        }
        if records.iter().any(|r| r.info.name == path) {
            return Err(format!(
                "{ASSET_SECTION_PREFIX}{path}: asset custom section appears more than once — a \
                 bundle must carry each asset path exactly once (ADR-0163 §3)"
            ));
        }

        let data = reader.data();
        let sha256: [u8; 32] = Sha256::digest(data).into();
        records.push(AssetRecord {
            info: AssetInfo { name: path.to_owned(), len: data.len() as u64, sha256 },
            offset: reader.data_offset(),
            len: data.len(),
        });
    }

    Ok(records)
}

/// The ADR-0163 §3 load window: payload access to a component's assets,
/// live only for the load window (`init` + `wire`). It holds the code blob
/// its opener brought and serves [`fetch`](Self::fetch) by streaming the
/// named asset's recorded range out of it, or
/// [`fetch_blob`](Self::fetch_blob) by viewing that range in place;
/// [`close`](Self::close) lets go of that blob so the payload path ends with
/// the window, while the catalog metadata is retained for the instance's
/// life (the ADR's "catalog for life, payload for the window" split).
pub struct LoadWindow {
    /// Each asset's name and payload range in `source`.
    sections: Vec<AssetSection>,
    /// The module's wasm bytes, held only while the window is open: the
    /// code the load or republish that opened it checked the module in
    /// from. `None` for an instance spawned from its publication, which
    /// brought no bytes, and once [`close`](Self::close)d.
    source: Option<Blob>,
    open: bool,
    /// Catalog metadata — retained for the instance's life, survives close.
    catalog: Vec<AssetInfo>,
}

impl LoadWindow {
    /// Open the window over `module`'s assets, reading payloads from
    /// `source`: the very code blob `module` was checked in from, or `None`
    /// when the opener brought none. A module with no asset sections opens a
    /// window that never serves a payload.
    #[must_use]
    pub fn open(module: &Module, source: Option<Blob>) -> Self {
        let manifest = module.manifest();
        Self {
            sections: manifest.asset_sections().to_vec(),
            source,
            open: true,
            catalog: manifest.asset_catalog().to_vec(),
        }
    }

    /// The asset catalog (metadata) as owned entries — for handing into
    /// [`aether_kinds::ComponentCapabilities::assets`] so it surfaces
    /// through `describe_component`.
    #[must_use]
    pub fn catalog(&self) -> Vec<AssetInfo> {
        self.catalog.clone()
    }

    /// The bytes of the asset named `name`: `Ok(None)` when the module
    /// carries no such asset or the window has closed.
    ///
    /// # Errors
    ///
    /// The asset is in the catalog but the window has no code to read it
    /// from, because the instance was spawned from its publication rather
    /// than loaded; or the code ends before the asset's recorded range.
    pub fn fetch(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let Some((range, source)) = self.locate(name)? else {
            return Ok(None);
        };
        read_range(source, range.clone()).map(Some).ok_or_else(|| ends_early(name, &range))
    }

    /// The asset named `name` as a blob that views its range of the module's
    /// code in place, copying nothing: `Ok(None)` when the module carries no
    /// such asset or the window has closed. The blob holds the code's store
    /// entry, so the code stays resident while the blob lives, past
    /// [`close`](Self::close).
    ///
    /// Code the opener brought as `Owned` bytes is checked in on the first
    /// call, and the window reads the resident entry from then on.
    ///
    /// # Errors
    ///
    /// As [`fetch`](Self::fetch); or the code cannot be read into the store.
    pub fn fetch_blob(&mut self, blobs: &BlobCheckIn, name: &str) -> Result<Option<Blob>, String> {
        let Some((range, source)) = self.locate(name)? else {
            return Ok(None);
        };
        let resident = blobs
            .entry(source)
            .map_err(|error| format!("`{name}`: the module's code cannot be read: {error}"))?
            .into_blob();
        let asset = blobs.view(&resident, range.clone()).ok_or_else(|| ends_early(name, &range));
        self.source = Some(resident);

        asset.map(Some)
    }

    /// Where the asset named `name` sits and the code to read it from:
    /// `Ok(None)` when the module carries no such asset or the window has
    /// closed.
    ///
    /// # Errors
    ///
    /// The asset is in the catalog but the window has no code to read it
    /// from, because the instance was spawned from its publication rather
    /// than loaded.
    fn locate(&self, name: &str) -> Result<Option<(Range<usize>, &Blob)>, String> {
        if !self.open {
            return Ok(None);
        }
        let Some(section) = self.sections.iter().find(|section| section.name.as_str() == name) else {
            return Ok(None);
        };
        let Some(source) = self.source.as_ref() else {
            return Err(format!(
                "`{name}` is in this module's asset catalog, but this instance was spawned from its \
                 publication, which keeps no bytes; load it with load_component to read its assets \
                 (ADR-0163 §4)"
            ));
        };
        Ok(Some((section.range.clone(), source)))
    }

    /// Close the window (ADR-0163 §3): let go of the module's code so
    /// neither [`fetch`](Self::fetch) nor [`fetch_blob`](Self::fetch_blob)
    /// serves — the substrate calls this when `wire` returns. The catalog
    /// metadata is retained. Idempotent.
    pub fn close(&mut self) {
        self.open = false;
        self.source = None;
    }

    /// Whether the payload window is still open (`false` after
    /// [`close`](Self::close)).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }
}

/// The bytes of `range` in `source`, streamed into a buffer of its exact
/// length, or `None` when `source` ends first.
fn read_range(source: &Blob, range: Range<usize>) -> Option<Vec<u8>> {
    let reader = BlobReader::open(source);
    let mut bytes = vec![0; range.len()];
    let mut filled = 0;
    while filled < bytes.len() {
        let copied = reader.read_range((range.start + filled) as u64, &mut bytes[filled..]);
        if copied == 0 {
            return None;
        }
        filled += copied;
    }
    Some(bytes)
}

/// The error for an asset whose recorded range runs past the module's code.
fn ends_early(name: &str, range: &Range<usize>) -> String {
    format!("`{name}`: the module's code ends before the asset's recorded range {range:?}")
}

impl AssetCatalog for LoadWindow {
    fn assets(&self) -> &[AssetInfo] {
        &self.catalog
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
mod tests {
    use std::sync::Arc;

    use wasmtime::Engine;

    use super::*;
    use crate::actor::wasm::module::ModuleCache;
    use crate::store::BlobStore;

    /// Build a module carrying `sections` as `(name, bytes)` custom
    /// sections, via WAT `@custom` (the `kind_manifest` idiom).
    fn wasm_with_sections(sections: &[(&str, &[u8])]) -> Vec<u8> {
        use core::fmt::Write as _;
        let mut customs = String::new();
        for (name, bytes) in sections {
            let mut escaped = String::with_capacity(bytes.len() * 4);
            for b in *bytes {
                write!(&mut escaped, "\\{b:02x}").expect("write to String");
            }
            write!(&mut customs, r#"(@custom "{name}" "{escaped}")"#).expect("write to String");
        }
        wat::parse_str(format!(r#"(module {customs} (func (export "noop")))"#)).unwrap()
    }

    /// A fresh engine's module cache, and a check-in handle over a fresh
    /// store whose resident bytes the caller reads.
    fn engine() -> (ModuleCache, BlobCheckIn, BlobStore) {
        let store = BlobStore::new().unwrap();
        (ModuleCache::new(Arc::new(Engine::default())), BlobCheckIn::new(store.clone()), store)
    }

    #[test]
    fn indexes_name_len_sha256_and_range() {
        // Tripwire: the indexed len + sha256 are computed off the exact
        // section bytes, and the recorded range slices back to those bytes.
        // A drift in the section-name scheme, the range math, or the hash
        // input reds this against a known payload.
        let payload: &[u8] = b"slime-sprite-bytes";
        let wasm = wasm_with_sections(&[("aether.asset.sprites/slime.png", payload)]);

        let records = read_assets_from_bytes(&wasm).unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.info.name, "sprites/slime.png");
        assert_eq!(record.info.len, payload.len() as u64);
        let expected_sha: [u8; 32] = Sha256::digest(payload).into();
        assert_eq!(record.info.sha256, expected_sha);
        // The recorded range reads the exact payload back out of the module.
        assert_eq!(&wasm[record.offset..record.offset + record.len], payload);
    }

    #[test]
    fn ignores_non_asset_sections_and_empty_module() {
        let wasm = wasm_with_sections(&[("aether.kinds", &[1, 2, 3]), ("producers", &[4, 5])]);
        assert!(read_assets_from_bytes(&wasm).unwrap().is_empty());

        let bare = wat::parse_str(r#"(module (func (export "noop")))"#).unwrap();
        assert!(read_assets_from_bytes(&bare).unwrap().is_empty());
    }

    #[test]
    fn duplicate_asset_section_is_a_load_error() {
        // Defense in depth behind the export_asset! link guard (#3979):
        // two sections under one asset path fail the load loudly rather
        // than serving an ambiguous payload.
        let wasm = wasm_with_sections(&[("aether.asset.dup.txt", b"first"), ("aether.asset.dup.txt", b"second")]);
        let err = read_assets_from_bytes(&wasm).unwrap_err();
        assert!(err.contains("more than once"), "err was: {err}");
        assert!(err.contains("dup.txt"), "err was: {err}");
    }

    #[test]
    fn empty_asset_path_is_a_load_error() {
        let wasm = wasm_with_sections(&[("aether.asset.", b"orphan")]);
        let err = read_assets_from_bytes(&wasm).unwrap_err();
        assert!(err.contains("empty asset path"), "err was: {err}");
    }

    /// The ADR-0163 §3 split: while open, the window serves each asset's
    /// exact bytes from the code its opener brought, holding that code
    /// resident even after the opener dropped its own value; closing it
    /// (what the substrate does when `wire` returns) lets the code leave the
    /// store and ends payload access, while the catalog survives. It catches
    /// a window that keeps its source past close (a loaded bundle's payload
    /// resident for the instance's life) and a read from the wrong range.
    #[test]
    fn window_serves_payload_then_close_lets_go_of_the_code() {
        let one: &[u8] = b"one";
        let two: &[u8] = b"two-longer";
        let wasm = wasm_with_sections(&[("aether.asset.a", one), ("aether.asset.b", two)]);
        let wasm_len = wasm.len();
        let (cache, blobs, store) = engine();
        let code = blobs.check_in(wasm.into_boxed_slice());
        let module = cache.check_in(&blobs, &code).unwrap();
        let mut window = LoadWindow::open(&module, Some(code));

        assert!(window.is_open());
        assert_eq!(window.fetch("a").unwrap().as_deref(), Some(one));
        assert_eq!(window.fetch("b").unwrap().as_deref(), Some(two));
        assert_eq!(window.fetch("missing").unwrap(), None);
        assert_eq!(store.resident_bytes(), wasm_len, "the open window alone holds the module's code");

        window.close();

        assert!(!window.is_open());
        assert_eq!(store.resident_bytes(), 0, "a closed window holds neither the code nor any payload");
        assert_eq!(window.fetch("a").unwrap(), None);
        assert_eq!(window.assets().len(), 2);
        assert_eq!(window.assets()[0].name, "a");
    }

    /// A second window over the same module, opened after the first closed
    /// and its code left the store, reads its assets again from the code
    /// its own opener brought: the module-cache hit answers the same entry,
    /// which kept no payload. It catches a window on a cache hit reading from
    /// a source the earlier window already released.
    #[test]
    fn a_window_opened_after_an_earlier_one_closed_serves_again() {
        let payload: &[u8] = b"slime-sprite-bytes";
        let wasm = wasm_with_sections(&[("aether.asset.slime", payload)]);
        let (cache, blobs, store) = engine();
        let code = blobs.check_in(wasm.clone().into_boxed_slice());
        let module = cache.check_in(&blobs, &code).unwrap();
        let mut first = LoadWindow::open(&module, Some(code));
        assert_eq!(first.fetch("slime").unwrap().as_deref(), Some(payload));
        first.close();
        assert_eq!(store.resident_bytes(), 0);

        let code = blobs.check_in(wasm.into_boxed_slice());
        let again = cache.check_in(&blobs, &code).unwrap();
        assert_eq!(again.hash(), module.hash());
        let second = LoadWindow::open(&again, Some(code));

        assert_eq!(second.fetch("slime").unwrap().as_deref(), Some(payload));
    }

    /// An instance spawned from its publication brings no code, so its
    /// window refuses a catalogued asset, naming the door that does bring
    /// it, while a name outside the catalog is still plain not-found. It
    /// catches a sourceless window answering a catalogued asset as missing.
    #[test]
    fn a_window_without_its_module_bytes_refuses_a_catalogued_asset() {
        let wasm = wasm_with_sections(&[("aether.asset.slime", b"slime")]);
        let (cache, blobs, _store) = engine();
        let module = cache.check_in(&blobs, &blobs.check_in(wasm.into_boxed_slice())).unwrap();
        let window = LoadWindow::open(&module, None);

        let error = window.fetch("slime").unwrap_err();
        assert!(error.contains("slime") && error.contains("load_component"), "error was: {error}");
        assert_eq!(window.fetch("missing").unwrap(), None);
        assert_eq!(window.assets().len(), 1, "the catalog answers without the bytes");
    }

    /// An open window's blob is the asset's exact bytes, viewed inside the
    /// code's own buffer, and it keeps the code resident after the window
    /// closed and the opener's value dropped, until the blob itself drops.
    /// It catches a blob that copies its range into a second resident
    /// buffer, one read from the wrong range, a view that lets the code go
    /// with the window, and code that stays resident after the last blob.
    #[test]
    fn a_window_blob_views_the_code_and_keeps_it_resident_until_it_drops() {
        let payload: &[u8] = b"slime-sprite-bytes";
        let wasm = wasm_with_sections(&[("aether.asset.other", b"other"), ("aether.asset.slime", payload)]);
        let wasm_len = wasm.len();
        let (cache, blobs, store) = engine();
        let code = blobs.check_in(wasm.into_boxed_slice());
        let module = cache.check_in(&blobs, &code).unwrap();
        let mut window = LoadWindow::open(&module, Some(code.clone()));

        let asset = window.fetch_blob(&blobs, "slime").unwrap().expect("the asset is in the catalog");
        let code_bytes = code.contiguous().expect("a store entry is contiguous").as_ptr_range();
        let asset_bytes = asset.contiguous().expect("a view is contiguous");

        assert_eq!(asset_bytes, payload);
        assert!(code_bytes.contains(&asset_bytes.as_ptr()), "the blob reads the code's buffer in place");
        assert_eq!(store.resident_bytes(), wasm_len, "a view adds no resident bytes");

        window.close();
        drop(code);

        assert_eq!(asset.contiguous(), Some(payload));
        assert_eq!(store.resident_bytes(), wasm_len, "the blob alone holds the module's code");

        drop(asset);

        assert_eq!(store.resident_bytes(), 0, "the code leaves with the last blob over it");
    }

    /// The window's refusals are the byte verb's: a closed window and a name
    /// outside the catalog are plain not-found, and a sourceless window
    /// refuses a catalogued asset naming `load_component`. It catches a blob
    /// served after `close`, and a sourceless window answering a catalogued
    /// asset as missing.
    #[test]
    fn a_window_blob_is_refused_as_the_bytes_are() {
        let wasm = wasm_with_sections(&[("aether.asset.slime", b"slime")]);
        let (cache, blobs, _store) = engine();
        let code = blobs.check_in(wasm.into_boxed_slice());
        let module = cache.check_in(&blobs, &code).unwrap();
        let mut sourceless = LoadWindow::open(&module, None);
        let mut window = LoadWindow::open(&module, Some(code));

        let error = sourceless.fetch_blob(&blobs, "slime").unwrap_err();
        assert!(error.contains("slime") && error.contains("load_component"), "error was: {error}");
        assert!(sourceless.fetch_blob(&blobs, "missing").unwrap().is_none());
        assert!(window.fetch_blob(&blobs, "missing").unwrap().is_none());

        window.close();

        assert!(window.fetch_blob(&blobs, "slime").unwrap().is_none());
    }

    /// Code that reached the window as `Owned` bytes, as a publish over the
    /// wire brings it, is checked in once: every blob views that one entry,
    /// and the byte verb reads it too. It catches a blob refused because its
    /// source is not yet a store entry, and a second copy of the code checked
    /// in per asset.
    #[test]
    fn a_window_over_owned_code_checks_it_in_once_for_its_blobs() {
        let wasm = wasm_with_sections(&[("aether.asset.a", b"one"), ("aether.asset.b", b"two-longer")]);
        let wasm_len = wasm.len();
        let (cache, blobs, store) = engine();
        let code = Blob::from(wasm);
        let module = cache.check_in(&blobs, &code).unwrap();
        let mut window = LoadWindow::open(&module, Some(code));
        assert_eq!(store.resident_bytes(), 0, "the cache kept no bytes, and the window's are not checked in");

        let one = window.fetch_blob(&blobs, "a").unwrap().expect("the asset is in the catalog");
        let two = window.fetch_blob(&blobs, "b").unwrap().expect("the asset is in the catalog");

        assert_eq!(one.contiguous(), Some(b"one".as_slice()));
        assert_eq!(two.contiguous(), Some(b"two-longer".as_slice()));
        assert_eq!(window.fetch("a").unwrap().as_deref(), Some(b"one".as_slice()));
        assert_eq!(store.resident_bytes(), wasm_len, "both blobs view one resident copy of the code");
    }

    #[test]
    fn assetless_module_opens_an_empty_window() {
        let bare = wat::parse_str(r#"(module (func (export "noop")))"#).unwrap();
        let (cache, blobs, _store) = engine();
        let code = blobs.check_in(bare.into_boxed_slice());
        let window = LoadWindow::open(&cache.check_in(&blobs, &code).unwrap(), Some(code));
        assert!(window.assets().is_empty());
        assert_eq!(window.fetch("anything").unwrap(), None);
    }
}
