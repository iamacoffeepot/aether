//! Bundle pack format: N components + chassis settings in one blob
//! (iamacoffeepot/aether#1529).
//!
//! `include_bytes!` needs a single fixed path, so embedding an
//! arbitrary component list into a standalone binary goes through a
//! *pack*: the crate's `build.rs` reads the bundle manifest named by
//! `AETHER_BUNDLE_MANIFEST` (a JSON [`BundleManifest`], written by
//! `cargo xtask bundle`), concatenates the listed wasm + config files
//! into one length-prefixed blob, and emits it to
//! `OUT_DIR/bundle_pack.bin`. The generic bundle bins
//! (`aether-bundle-desktop`, `aether-bundle-headless`) embed that one
//! blob and [`decode_pack`] it at boot into the chassis `autoload`
//! list plus chassis settings (title / window mode / tick rate).
//!
//! The binary layout is an internal build-time format — encoder and
//! decoder ship in the same commit, nothing on disk outlives a build —
//! not a stable interface (no ADR; revisit only if it ever becomes a
//! persisted artifact). The module is `include!`d by `build.rs`, so it
//! stays self-contained: std plus serde only, no crate-internal
//! imports.
//!
//! Layout (all integers little-endian): the 8-byte magic
//! `b"AEBNDLP1"`; the three optional chassis settings (`title`,
//! `window_mode` as optional strings, `tick_hz` as an optional u32); a
//! u32 component count; then per component the wasm bytes and config
//! bytes (each u64 length + data) and the optional `name` / `export`
//! strings. Optional fields are a presence byte (0/1) followed by the
//! value; strings are a u32 length + UTF-8 bytes.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::str;

/// Magic + version tag opening every pack blob.
pub const PACK_MAGIC: &[u8; 8] = b"AEBNDLP1";

/// One component embedded in a pack, in autoload order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackedComponent {
    /// The component's wasm bytes.
    pub wasm: Vec<u8>,
    /// Init-config bytes (ADR-0090); empty for none.
    pub config: Vec<u8>,
    /// Optional load name (`aether.component.load`'s `name`).
    pub name: Option<String>,
    /// Optional export selector (ADR-0096).
    pub export: Option<String>,
}

/// Chassis settings the bundle bins apply before `run()`. All
/// optional — an unset field keeps the chassis env's own resolution
/// (env vars / defaults). Fields a chassis doesn't support (desktop
/// has no `tick_hz`; headless has no window) are warn-ignored by the
/// non-matching bin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChassisSettings {
    /// Desktop window title.
    pub title: Option<String>,
    /// Desktop window mode spec, same vocabulary as
    /// `AETHER_WINDOW_MODE` (`windowed[:WxH]` / `fullscreen-borderless`
    /// / `exclusive:WxH@HZ`).
    pub window_mode: Option<String>,
    /// Headless tick cadence in hertz.
    pub tick_hz: Option<u32>,
}

/// A decoded pack: chassis settings plus the ordered component list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pack {
    pub chassis: ChassisSettings,
    pub components: Vec<PackedComponent>,
}

/// Pack decode failure. The pack is produced by this crate's own
/// `build.rs` in the same commit, so any of these indicates a
/// corrupted embed (or a bin built against a stale `OUT_DIR`), not a
/// user input error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackError {
    /// The blob doesn't open with [`PACK_MAGIC`].
    BadMagic,
    /// A length prefix points past the end of the blob.
    Truncated,
    /// A string field holds invalid UTF-8.
    BadUtf8,
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => write!(f, "bundle pack blob does not start with {PACK_MAGIC:?}"),
            Self::Truncated => write!(f, "bundle pack blob is truncated"),
            Self::BadUtf8 => write!(f, "bundle pack string field holds invalid UTF-8"),
        }
    }
}

impl Error for PackError {}

/// The JSON manifest `cargo xtask bundle` writes and `build.rs` reads
/// via `AETHER_BUNDLE_MANIFEST`. Paths are resolved by `build.rs`
/// relative to its working directory, so the writer uses absolute
/// paths. The xtask side serializes this shape with `serde_json::json!`
/// (xtask doesn't depend on this crate); keep the two in sync.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct BundleManifest {
    /// Which chassis bin the pack targets (`desktop` / `headless`).
    /// Bookkeeping for the writer; `build.rs` packs the same blob
    /// either way (the chassis choice selects the bin to build).
    #[serde(default)]
    pub chassis: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub window_mode: Option<String>,
    #[serde(default)]
    pub tick_hz: Option<u32>,
    /// Ordered component list — pack (and autoload) order is list order.
    pub components: Vec<ManifestComponent>,
}

/// One component entry in a [`BundleManifest`].
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ManifestComponent {
    /// Path to the built wasm artifact.
    pub wasm: PathBuf,
    /// Optional path to the init-config bytes file.
    #[serde(default)]
    pub config: Option<PathBuf>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub export: Option<String>,
}

/// A failure reading a [`BundleManifest`] (or a file it names) into a
/// [`Pack`]. Shared by `build.rs` (compile-time embed — it `panic!`s
/// on this) and the chassis runtime boot-manifest reader (which maps
/// it to a hard config fault, ADR-0090 §4). Each variant carries the
/// offending path so the message names the file, not just the fault.
#[derive(Debug)]
pub enum ManifestError {
    /// The manifest JSON file could not be read off disk.
    ReadManifest { path: PathBuf, source: io::Error },
    /// The manifest JSON did not parse into a [`BundleManifest`].
    ParseManifest {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// A component's wasm artifact could not be read.
    ReadWasm { path: PathBuf, source: io::Error },
    /// A component's init-config file could not be read.
    ReadConfig { path: PathBuf, source: io::Error },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadManifest { path, source } => {
                write!(f, "read bundle manifest from {}: {source}", path.display())
            }
            Self::ParseManifest { path, source } => {
                write!(f, "parse bundle manifest at {}: {source}", path.display())
            }
            Self::ReadWasm { path, source } => {
                write!(f, "read component wasm from {}: {source}", path.display())
            }
            Self::ReadConfig { path, source } => {
                write!(f, "read component config from {}: {source}", path.display())
            }
        }
    }
}

impl Error for ManifestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadManifest { source, .. }
            | Self::ReadWasm { source, .. }
            | Self::ReadConfig { source, .. } => Some(source),
            Self::ParseManifest { source, .. } => Some(source),
        }
    }
}

/// Read and parse the JSON [`BundleManifest`] at `manifest_path`. Split
/// out from [`pack_from_manifest`] so `build.rs` can walk the parsed
/// component list to register each input for `cargo:rerun-if-changed`
/// before the bytes are read.
///
/// # Errors
///
/// Returns [`ManifestError::ReadManifest`] / [`ManifestError::ParseManifest`]
/// when the file is unreadable or its JSON doesn't match the schema.
pub fn read_manifest(manifest_path: &Path) -> Result<BundleManifest, ManifestError> {
    let json = fs::read_to_string(manifest_path).map_err(|source| ManifestError::ReadManifest {
        path: manifest_path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&json).map_err(|source| ManifestError::ParseManifest {
        path: manifest_path.to_path_buf(),
        source,
    })
}

/// Read the manifest at `manifest_path` plus every wasm / config file it
/// names into a [`Pack`]. Pure file I/O — emits no cargo directives, so
/// the same reader serves the compile-time embed (`build.rs`, which
/// registers `rerun-if-changed` itself) and the runtime boot-manifest
/// path (`AETHER_BOOT_MANIFEST`, read in the chassis `from_env_with_argv`).
/// Paths in the manifest are resolved as-is (absolute, per the writer
/// contract).
///
/// # Errors
///
/// Returns a [`ManifestError`] when the manifest, a component wasm, or a
/// component config file can't be read or parsed.
pub fn pack_from_manifest(manifest_path: &Path) -> Result<Pack, ManifestError> {
    let manifest = read_manifest(manifest_path)?;
    let mut components = Vec::with_capacity(manifest.components.len());
    for entry in manifest.components {
        let wasm = fs::read(&entry.wasm).map_err(|source| ManifestError::ReadWasm {
            path: entry.wasm.clone(),
            source,
        })?;
        let config = match entry.config.as_ref() {
            Some(path) => fs::read(path).map_err(|source| ManifestError::ReadConfig {
                path: path.clone(),
                source,
            })?,
            None => Vec::new(),
        };
        components.push(PackedComponent {
            wasm,
            config,
            name: entry.name,
            export: entry.export,
        });
    }
    Ok(Pack {
        chassis: ChassisSettings {
            title: manifest.title,
            window_mode: manifest.window_mode,
            tick_hz: manifest.tick_hz,
        },
        components,
    })
}

fn put_bytes_long(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn put_opt_string(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(s) => {
            let len = u32::try_from(s.len()).expect("pack string length fits in 32 bits");
            out.push(1);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        None => out.push(0),
    }
}

/// Encode `pack` into the single blob the bundle bins embed.
///
/// # Panics
///
/// Panics if a string field or the component count exceeds 32 bits of
/// length — unreachable for any real bundle input.
#[must_use]
pub fn encode_pack(pack: &Pack) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(PACK_MAGIC);
    put_opt_string(&mut out, pack.chassis.title.as_deref());
    put_opt_string(&mut out, pack.chassis.window_mode.as_deref());
    match pack.chassis.tick_hz {
        Some(hz) => {
            out.push(1);
            out.extend_from_slice(&hz.to_le_bytes());
        }
        None => out.push(0),
    }
    let count = u32::try_from(pack.components.len()).expect("pack component count fits in 32 bits");
    out.extend_from_slice(&count.to_le_bytes());
    for component in &pack.components {
        put_bytes_long(&mut out, &component.wasm);
        put_bytes_long(&mut out, &component.config);
        put_opt_string(&mut out, component.name.as_deref());
        put_opt_string(&mut out, component.export.as_deref());
    }
    out
}

/// Byte-slice cursor for the iterative decoder.
struct Cursor<'a> {
    rest: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], PackError> {
        if len > self.rest.len() {
            return Err(PackError::Truncated);
        }
        let (head, tail) = self.rest.split_at(len);
        self.rest = tail;
        Ok(head)
    }

    fn take_byte(&mut self) -> Result<u8, PackError> {
        Ok(self.take(1)?[0])
    }

    fn take_short_len(&mut self) -> Result<usize, PackError> {
        let raw = self.take(4)?;
        let len = u32::from_le_bytes(raw.try_into().expect("4-byte slice"));
        Ok(len as usize)
    }

    fn take_long_len(&mut self) -> Result<usize, PackError> {
        let raw = self.take(8)?;
        let len = u64::from_le_bytes(raw.try_into().expect("8-byte slice"));
        usize::try_from(len).map_err(|_| PackError::Truncated)
    }

    fn take_bytes_long(&mut self) -> Result<Vec<u8>, PackError> {
        let len = self.take_long_len()?;
        Ok(self.take(len)?.to_vec())
    }

    fn take_opt_string(&mut self) -> Result<Option<String>, PackError> {
        if self.take_byte()? == 0 {
            return Ok(None);
        }
        let len = self.take_short_len()?;
        let bytes = self.take(len)?;
        let s = str::from_utf8(bytes).map_err(|_| PackError::BadUtf8)?;
        Ok(Some(s.to_owned()))
    }
}

/// Decode a pack blob produced by [`encode_pack`].
///
/// # Errors
///
/// Returns a [`PackError`] when the blob's magic, a length prefix, or
/// a string field doesn't decode — see the variant docs.
///
/// # Panics
///
/// Never in practice: the only `expect`s convert `Cursor::take` slices
/// of a fixed length into same-length arrays.
pub fn decode_pack(bytes: &[u8]) -> Result<Pack, PackError> {
    let mut cursor = Cursor { rest: bytes };
    if cursor.take(PACK_MAGIC.len())? != PACK_MAGIC {
        return Err(PackError::BadMagic);
    }
    let title = cursor.take_opt_string()?;
    let window_mode = cursor.take_opt_string()?;
    let tick_hz = if cursor.take_byte()? == 0 {
        None
    } else {
        let raw = cursor.take(4)?;
        Some(u32::from_le_bytes(raw.try_into().expect("4-byte slice")))
    };
    let count = cursor.take_short_len()?;
    let mut components = Vec::new();
    for _ in 0..count {
        let wasm = cursor.take_bytes_long()?;
        let config = cursor.take_bytes_long()?;
        let name = cursor.take_opt_string()?;
        let export = cursor.take_opt_string()?;
        components.push(PackedComponent {
            wasm,
            config,
            name,
            export,
        });
    }
    Ok(Pack {
        chassis: ChassisSettings {
            title,
            window_mode,
            tick_hz,
        },
        components,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pack() -> Pack {
        Pack {
            chassis: ChassisSettings {
                title: Some("loco-motion".to_owned()),
                window_mode: Some("windowed:1280x720".to_owned()),
                tick_hz: Some(120),
            },
            components: vec![
                PackedComponent {
                    wasm: vec![0x00, 0x61, 0x73, 0x6d],
                    config: vec![1, 2, 3],
                    name: Some("first".to_owned()),
                    export: None,
                },
                PackedComponent {
                    wasm: vec![0xfe, 0xff],
                    config: Vec::new(),
                    name: None,
                    export: Some("alt".to_owned()),
                },
            ],
        }
    }

    #[test]
    fn round_trip_preserves_order_bytes_and_settings() {
        let pack = sample_pack();
        let decoded = decode_pack(&encode_pack(&pack)).expect("decode");
        assert_eq!(decoded, pack);
    }

    #[test]
    fn round_trip_empty_pack() {
        // The placeholder a plain `cargo build --workspace` embeds: no
        // chassis settings, no components.
        let pack = Pack::default();
        let decoded = decode_pack(&encode_pack(&pack)).expect("decode");
        assert_eq!(decoded, pack);
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut bytes = encode_pack(&Pack::default());
        bytes[0] ^= 0xff;
        assert_eq!(decode_pack(&bytes), Err(PackError::BadMagic));
    }

    #[test]
    fn decode_rejects_truncation_at_every_length() {
        // Chopping the encoded sample anywhere short of its full length
        // must error (Truncated everywhere except inside a string body,
        // where the cut can land mid-UTF-8 — either way it's an Err).
        let bytes = encode_pack(&sample_pack());
        for len in 0..bytes.len() {
            assert!(
                decode_pack(&bytes[..len]).is_err(),
                "decode of {len}-byte prefix unexpectedly succeeded",
            );
        }
    }

    #[test]
    fn manifest_json_round_trips() {
        // The schema `cargo xtask bundle` writes with `serde_json::json!`
        // — field names here are the contract the xtask side mirrors.
        let json = r#"{
            "chassis": "headless",
            "tick_hz": 30,
            "components": [
                {"wasm": "/abs/a.wasm", "config": "/abs/a.cfg", "name": "a"},
                {"wasm": "/abs/b.wasm"}
            ]
        }"#;
        let manifest: BundleManifest = serde_json::from_str(json).expect("parse manifest");
        assert_eq!(manifest.chassis.as_deref(), Some("headless"));
        assert_eq!(manifest.title, None);
        assert_eq!(manifest.tick_hz, Some(30));
        assert_eq!(manifest.components.len(), 2);
        assert_eq!(manifest.components[0].name.as_deref(), Some("a"));
        assert_eq!(manifest.components[1].config, None);
    }

    /// A per-test scratch directory under the system temp dir, unique
    /// per call so concurrent test threads never collide.
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::env;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-bundle-pack-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn pack_from_manifest_reads_wasm_and_config_in_order() {
        // The runtime boot-manifest path (and `build.rs`) read the
        // listed files into a `Pack`; this proves the reader resolves
        // each manifest entry's wasm + optional config and preserves
        // list order, name, and export.
        let dir = scratch_dir("read");
        let wasm_a = dir.join("a.wasm");
        let cfg_a = dir.join("a.cfg");
        let wasm_b = dir.join("b.wasm");
        fs::write(&wasm_a, [0x00, 0x61, 0x73, 0x6d]).expect("write a.wasm");
        fs::write(&cfg_a, [1, 2, 3]).expect("write a.cfg");
        fs::write(&wasm_b, [0xfe, 0xff]).expect("write b.wasm");
        let manifest_path = dir.join("manifest.json");
        let manifest_json = serde_json::json!({
            "tick_hz": 30,
            "components": [
                {"wasm": wasm_a, "config": cfg_a, "name": "first"},
                {"wasm": wasm_b, "export": "alt"},
            ],
        });
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest_json).expect("serialize manifest"),
        )
        .expect("write manifest");

        let pack = pack_from_manifest(&manifest_path).expect("read pack");
        assert_eq!(pack.chassis.tick_hz, Some(30));
        assert_eq!(
            pack.components,
            vec![
                PackedComponent {
                    wasm: vec![0x00, 0x61, 0x73, 0x6d],
                    config: vec![1, 2, 3],
                    name: Some("first".to_owned()),
                    export: None,
                },
                PackedComponent {
                    wasm: vec![0xfe, 0xff],
                    config: Vec::new(),
                    name: None,
                    export: Some("alt".to_owned()),
                },
            ],
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pack_from_manifest_errors_on_missing_wasm() {
        // A manifest naming a wasm that isn't on disk is a hard read
        // error — the chassis maps this to an aborting config fault.
        let dir = scratch_dir("missing");
        let manifest_path = dir.join("manifest.json");
        let manifest_json = serde_json::json!({
            "components": [{"wasm": dir.join("nope.wasm")}],
        });
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest_json).expect("serialize manifest"),
        )
        .expect("write manifest");

        let err = pack_from_manifest(&manifest_path).expect_err("missing wasm errors");
        assert!(matches!(err, ManifestError::ReadWasm { .. }), "{err:?}");

        fs::remove_dir_all(&dir).ok();
    }
}
