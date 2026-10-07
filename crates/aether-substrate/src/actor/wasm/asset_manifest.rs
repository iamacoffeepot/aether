//! ADR-0250 asset section indexer (#3969).
//!
//! A component that ships assets declares them with `export_asset!`
//! (ADR-0163 §2, #3979), which lands each asset's bytes in a wasm custom
//! section named `aether.asset.<path>` — the same emission path as the
//! `aether.kinds` sections (see [`super::kind_manifest`]), never
//! instantiated by wasm execution. This module reads those sections
//! host-side before compilation, exactly as `kind_manifest` reads
//! `aether.kinds`: [`read_assets_from_bytes`] walks the raw bytes with
//! `wasmparser` and records, per asset, its catalog entry ([`AssetInfo`]:
//! name, length) plus the byte range of its payload within the module file.
//! The module cache checks each record's slice in as the asset's own blob,
//! so a publish holds one deduplicated blob per asset for as long as a
//! `Module` holds it (ADR-0250 §1, §2, [`super::module`]).
//!
//! The catalog is metadata, kept for the instance's life; it surfaces
//! through `describe_component`
//! ([`aether_kinds::ComponentCapabilities::assets`]). Payload access is the
//! module's asset blob, served in every hook from the instance's own module.
//!
//! This reads the custom sections only and never looks at linear memory.
//! `export_asset!` keeps the payload out of linear memory on its own (it
//! withholds the `#[used]` pin, so the dead `#[link_section]` static is
//! garbage-collected before the shipped wasm; #3981), and this indexer
//! would be correct either way. Do not read a "not in linear memory"
//! property out of this module; it makes no such claim.

use aether_kinds::AssetInfo;
use rustc_hash::FxHashSet;
use wasmparser::{Parser, Payload};

/// The prefix every asset custom section's name carries (ADR-0163 §2). An
/// asset's catalog name is its section name with this prefix stripped.
pub const ASSET_SECTION_PREFIX: &str = "aether.asset.";

/// One indexed asset: its catalog entry ([`AssetInfo`]) plus the byte
/// range of its payload within the module file. The range is what the module
/// cache checks in as the asset's own blob (ADR-0250 §1).
#[derive(Debug, Clone)]
pub struct AssetRecord {
    /// Catalog metadata: name and length.
    pub info: AssetInfo,
    /// Offset of the asset's payload bytes within the module file.
    pub offset: usize,
    /// Length of the asset's payload bytes (equals `info.len`).
    pub len: usize,
}

/// Walk a module's `aether.asset.*` custom sections and index each asset
/// (ADR-0163 §3): catalog name (the section-name suffix after
/// [`ASSET_SECTION_PREFIX`]), byte length, and byte range into
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
    let mut seen: FxHashSet<&str> = FxHashSet::default();

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
        if !seen.insert(path) {
            return Err(format!(
                "{ASSET_SECTION_PREFIX}{path}: asset custom section appears more than once — a \
                 bundle must carry each asset path exactly once (ADR-0163 §3)"
            ));
        }

        let data = reader.data();
        records.push(AssetRecord {
            info: AssetInfo { name: path.to_owned(), len: data.len() as u64 },
            offset: reader.data_offset(),
            len: data.len(),
        });
    }

    Ok(records)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
mod tests {
    use std::sync::Arc;

    use aether_data::Blob;
    use wasmtime::Engine;

    use super::*;
    use crate::actor::native::BlobCheckIn;
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
    fn indexes_name_len_and_range() {
        // Tripwire: the recorded range slices back to the exact section
        // bytes. A drift in the section-name scheme or the range math reds
        // this against a known payload.
        let payload: &[u8] = b"slime-sprite-bytes";
        let wasm = wasm_with_sections(&[("aether.asset.sprites/slime.png", payload)]);

        let records = read_assets_from_bytes(&wasm).unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.info.name, "sprites/slime.png");
        assert_eq!(record.info.len, payload.len() as u64);
        // The recorded range reads the exact payload back out of the module.
        assert_eq!(&wasm[record.offset..record.offset + record.len], payload);
    }

    /// A module of many assets with distinct payloads lists them in section
    /// order and serves each name's own payload from its own blob. It catches
    /// a catalog built by walking the name map (hash order, which a two-asset
    /// module can pass by luck) and a map whose positions disagree with the
    /// lists.
    #[test]
    fn many_assets_keep_section_order_and_fetch_their_own_payloads() {
        let names: Vec<String> = (0..64).map(|n| format!("bundle/asset-{n:03}.bin")).collect();
        let payloads: Vec<Vec<u8>> = (0..64u8).map(|n| vec![n; usize::from(n) + 1]).collect();
        let sections: Vec<(String, &[u8])> = names
            .iter()
            .zip(&payloads)
            .map(|(name, bytes)| (format!("aether.asset.{name}"), bytes.as_slice()))
            .collect();
        let borrowed: Vec<(&str, &[u8])> = sections.iter().map(|(name, bytes)| (name.as_str(), *bytes)).collect();
        let (cache, blobs, _store) = engine();
        let code = blobs.check_in(wasm_with_sections(&borrowed).into_boxed_slice());
        let module = cache.check_in(&blobs, &code).unwrap();

        let catalog: Vec<(&str, u64)> =
            module.manifest().asset_catalog().iter().map(|info| (info.name.as_str(), info.len)).collect();
        let expected: Vec<(&str, u64)> =
            names.iter().zip(&payloads).map(|(name, bytes)| (name.as_str(), bytes.len() as u64)).collect();
        assert_eq!(catalog, expected);
        for (name, payload) in names.iter().zip(&payloads) {
            let section = module.manifest().assets().section(name).expect("the asset is indexed");
            assert_eq!(section.blob.contiguous(), Some(payload.as_slice()));
        }
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

    /// Code that reached the module as `Owned` bytes, as a publish over the
    /// wire brings it, still serves its assets from the checked-in blobs.
    #[test]
    fn a_module_checked_in_from_owned_bytes_serves_its_assets() {
        let wasm = wasm_with_sections(&[("aether.asset.a", b"one"), ("aether.asset.b", b"two-longer")]);
        let (cache, blobs, _store) = engine();
        let code = Blob::from(wasm);
        let module = cache.check_in(&blobs, &code).unwrap();

        for (name, expected) in [("a", b"one".as_slice()), ("b", b"two-longer".as_slice())] {
            let section = module.manifest().assets().section(name).expect("the asset is indexed");
            assert_eq!(section.blob.contiguous(), Some(expected));
        }
        assert!(module.manifest().assets().section("missing").is_none());
    }

    #[test]
    fn assetless_module_has_an_empty_catalog() {
        let bare = wat::parse_str(r#"(module (func (export "noop")))"#).unwrap();
        let (cache, blobs, _store) = engine();
        let code = blobs.check_in(bare.into_boxed_slice());
        let module = cache.check_in(&blobs, &code).unwrap();
        assert!(module.manifest().asset_catalog().is_empty());
        assert!(module.manifest().assets().section("anything").is_none());
    }
}
