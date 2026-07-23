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
//! module file.
//!
//! The catalog is metadata — a few hundred bytes — so it is kept for the
//! instance's life and surfaces through `describe_component`
//! ([`aether_kinds::ComponentCapabilities::assets`]). Payload access is
//! the [`LoadWindow`]: it pins the module bytes and serves `asset(name)`
//! by slicing the recorded range, and [`LoadWindow::close`] releases both
//! the pin and the ranges when the load window ends (`init` + `wire`), so
//! nothing payload-sized outlives the window and store eviction can never
//! strand a live actor (ADR-0163 §3/§4).
//!
//! This reads the custom sections only and never looks at linear memory.
//! `export_asset!` keeps the payload out of linear memory on its own (it
//! withholds the `#[used]` pin, so the dead `#[link_section]` static is
//! garbage-collected before the shipped wasm; #3981), and this indexer
//! would be correct either way. Do not read a "not in linear memory"
//! property out of this module; it makes no such claim.

use std::ops::Range;
use std::sync::Arc;

use aether_actor::{AssetCatalog, AssetWindow};
use aether_kinds::AssetInfo;
use sha2::{Digest, Sha256};
use wasmparser::{Parser, Payload};

/// The prefix every asset custom section's name carries (ADR-0163 §2). An
/// asset's catalog name is its section name with this prefix stripped.
pub const ASSET_SECTION_PREFIX: &str = "aether.asset.";

/// One indexed asset: its catalog entry ([`AssetInfo`]) plus the byte
/// range of its payload within the module file. The range lets the
/// [`LoadWindow`] read the bytes straight from the module without staging
/// a second copy (ADR-0163 §3).
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
/// live only for the load window (`init` + `wire`). It pins the module
/// bytes and serves [`asset`](AssetWindow::asset) by slicing the recorded
/// range; [`close`](Self::close) drops the pin and the ranges so the
/// payload path ends with the window, while the catalog metadata is
/// retained for the instance's life (the ADR's "catalog for life, payload
/// for the window" split). Serving from the already-in-hand module bytes
/// is this slice's v1 of the ADR's "read the recorded range from the store
/// file": nothing payload-sized outlives the window either way.
pub struct LoadWindow {
    /// Module bytes, pinned only while the window is open; `None` once
    /// [`close`](Self::close)d.
    wasm: Option<Arc<[u8]>>,
    /// Catalog metadata — retained for the instance's life, survives close.
    catalog: Vec<AssetInfo>,
    /// Payload byte range per catalog entry (parallel to `catalog`),
    /// emptied on close so payload access ends with the window.
    ranges: Vec<Range<usize>>,
}

impl LoadWindow {
    /// Index `wasm`'s asset sections and open the window over them. Errors
    /// exactly as [`read_assets_from_bytes`] (duplicate / empty asset
    /// path). A module with no asset sections opens an empty window whose
    /// `asset` always returns `None`.
    pub fn index(wasm: Arc<[u8]>) -> Result<Self, String> {
        let records = read_assets_from_bytes(&wasm)?;
        let catalog = records.iter().map(|r| r.info.clone()).collect();
        let ranges = records.iter().map(|r| r.offset..r.offset + r.len).collect();
        Ok(Self { wasm: Some(wasm), catalog, ranges })
    }

    /// The asset catalog (metadata) as owned entries — for handing into
    /// [`aether_kinds::ComponentCapabilities::assets`] so it surfaces
    /// through `describe_component`.
    #[must_use]
    pub fn catalog(&self) -> Vec<AssetInfo> {
        self.catalog.clone()
    }

    /// Close the window (ADR-0163 §3): drop the module-bytes pin and the
    /// payload ranges so [`asset`](AssetWindow::asset) no longer serves —
    /// the substrate calls this when `wire` returns. The catalog metadata
    /// is retained. Idempotent.
    pub fn close(&mut self) {
        self.wasm = None;
        self.ranges.clear();
    }

    /// Whether the payload window is still open (`false` after
    /// [`close`](Self::close)).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.wasm.is_some()
    }
}

impl AssetCatalog for LoadWindow {
    fn assets(&self) -> &[AssetInfo] {
        &self.catalog
    }
}

impl AssetWindow for LoadWindow {
    fn asset(&mut self, name: &str) -> Option<Vec<u8>> {
        let wasm = self.wasm.as_ref()?;
        let idx = self.catalog.iter().position(|entry| entry.name == name)?;
        let range = self.ranges.get(idx)?.clone();
        wasm.get(range).map(<[u8]>::to_vec)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
mod tests {
    use super::*;

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

    #[test]
    fn window_serves_payload_then_close_ends_access() {
        // The ADR-0163 §3 split: the window serves payload bytes while
        // open; closing it (what the substrate does when `wire` returns)
        // ends payload access, while the catalog metadata is retained for
        // the instance's life.
        let one: &[u8] = b"one";
        let two: &[u8] = b"two-longer";
        let wasm = wasm_with_sections(&[("aether.asset.a", one), ("aether.asset.b", two)]);
        let mut window = LoadWindow::index(Arc::from(wasm.into_boxed_slice())).unwrap();

        assert!(window.is_open());
        assert_eq!(window.asset("a").as_deref(), Some(one));
        assert_eq!(window.asset("b").as_deref(), Some(two));
        assert_eq!(window.asset("missing"), None);
        // Catalog reports both regardless of read order.
        assert_eq!(window.assets().len(), 2);

        window.close();

        assert!(!window.is_open());
        // Payload access is gone even for a name that was served above.
        assert_eq!(window.asset("a"), None);
        // Catalog metadata survives the window for the instance's life.
        assert_eq!(window.assets().len(), 2);
        assert_eq!(window.assets()[0].name, "a");
    }

    #[test]
    fn assetless_module_opens_an_empty_window() {
        let bare = wat::parse_str(r#"(module (func (export "noop")))"#).unwrap();
        let mut window = LoadWindow::index(Arc::from(bare.into_boxed_slice())).unwrap();
        assert!(window.assets().is_empty());
        assert_eq!(window.asset("anything"), None);
    }
}
