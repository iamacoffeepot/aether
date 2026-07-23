//! Boot manifest format: the JSON [`BootManifest`] of component file paths
//! plus the [`PackedComponent`] / [`ChassisSettings`] types the boot autoload
//! path shares.
//!
//! The hub's `spawn_substrate` boot-manifest injection describes a component
//! set as a JSON [`BootManifest`] — an ordered list of wasm + optional config
//! file *paths* plus the three chassis knobs (title / window mode / tick
//! rate). It reaches the spawned chassis through `AETHER_BOOT_MANIFEST`
//! (`--boot-manifest`); the substrate-boot and autoload tests write the same
//! shape directly. [`pack_from_manifest`] reads that manifest and the files it
//! names into a [`Pack`], which the chassis autoload
//! (`crate::autoload::boot_manifest_autoload`) drains into
//! `aether.component.load` mail.
//!
//! The persisted, content-addressed shipping channel — the package depot
//! `cargo xtask package` emits — lives in [`crate::package`] (ADR-0163 §1): a
//! versioned `pack/manifest` referencing objects by hash rather than naming
//! them by path. That channel and this one share the [`PackedComponent`] and
//! [`ChassisSettings`] autoload types defined here.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

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
    /// Optional instance count (issue 2626): fan this entry out into N
    /// autoload components, named `{base}-{index}`, at expansion time
    /// (`autoload::expand_replicas`). `None` (or the historical absence of
    /// this field) keeps today's one-instance behaviour.
    pub replicas: Option<u32>,
}

/// Chassis settings a package applies at boot. The depot boot path
/// (`--package` / `AETHER_PACKAGE`) overlays them BELOW argv/env, ABOVE the
/// compiled defaults (issue 4001), so an operator can still override a
/// shipped package. All optional — an unset field keeps the chassis env's own
/// resolution (env vars / defaults). Fields a chassis doesn't support
/// (desktop has no `tick_hz`; headless has no window) are warn-ignored by the
/// non-matching chassis.
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

/// The JSON manifest the hub's `spawn_substrate` boot-manifest injection
/// writes and the chassis boot path reads via `AETHER_BOOT_MANIFEST` /
/// `--boot-manifest`. Paths are resolved as-is, so the writer uses absolute
/// paths. The hub serializes this shape with `serde_json::json!` (it doesn't
/// depend on this crate); keep the two in sync.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct BootManifest {
    /// Which chassis the manifest targets (`desktop` / `headless`).
    /// Bookkeeping for the writer; the reader drains the same component
    /// list either way (the chassis choice selects which bin was spawned).
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

/// One component entry in a [`BootManifest`].
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
    /// Optional instance count (issue 2626): expanded into N autoload
    /// components at read time, one shared config, names `{base}-{index}`.
    /// `replicas: 0` is a hard config error, not a silent no-op.
    #[serde(default)]
    pub replicas: Option<u32>,
}

/// A failure reading a [`BootManifest`] (or a file it names) into a
/// [`Pack`]. Surfaces from the chassis runtime boot-manifest reader, which
/// maps it to a hard config fault (ADR-0090 §4). Each variant carries the
/// offending path so the message names the file, not just the fault.
#[derive(Debug)]
pub enum ManifestError {
    /// The manifest JSON file could not be read off disk.
    ReadManifest { path: PathBuf, source: io::Error },
    /// The manifest JSON did not parse into a [`BootManifest`].
    ParseManifest { path: PathBuf, source: serde_json::Error },
    /// A component's wasm artifact could not be read.
    ReadWasm { path: PathBuf, source: io::Error },
    /// A component's init-config file could not be read.
    ReadConfig { path: PathBuf, source: io::Error },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadManifest { path, source } => {
                write!(f, "read boot manifest from {}: {source}", path.display())
            }
            Self::ParseManifest { path, source } => {
                write!(f, "parse boot manifest at {}: {source}", path.display())
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
            Self::ReadManifest { source, .. } | Self::ReadWasm { source, .. } | Self::ReadConfig { source, .. } => {
                Some(source)
            }
            Self::ParseManifest { source, .. } => Some(source),
        }
    }
}

/// Read and parse the JSON [`BootManifest`] at `manifest_path`. Split out
/// from [`pack_from_manifest`] so a caller can inspect the parsed component
/// list before the wasm / config bytes are read.
///
/// # Errors
///
/// Returns [`ManifestError::ReadManifest`] / [`ManifestError::ParseManifest`]
/// when the file is unreadable or its JSON doesn't match the schema.
pub fn read_manifest(manifest_path: &Path) -> Result<BootManifest, ManifestError> {
    let json = fs::read_to_string(manifest_path)
        .map_err(|source| ManifestError::ReadManifest { path: manifest_path.to_path_buf(), source })?;
    serde_json::from_str(&json)
        .map_err(|source| ManifestError::ParseManifest { path: manifest_path.to_path_buf(), source })
}

/// Read the manifest at `manifest_path` plus every wasm / config file it
/// names into a [`Pack`]. Pure file I/O — emits no cargo directives, so it
/// serves the runtime boot-manifest path (`AETHER_BOOT_MANIFEST`, read in the
/// chassis `resolve`). Paths in the manifest are resolved as-is (absolute, per
/// the writer contract).
///
/// # Errors
///
/// Returns a [`ManifestError`] when the manifest, a component wasm, or a
/// component config file can't be read or parsed.
pub fn pack_from_manifest(manifest_path: &Path) -> Result<Pack, ManifestError> {
    let manifest = read_manifest(manifest_path)?;
    let mut components = Vec::with_capacity(manifest.components.len());
    for entry in manifest.components {
        let wasm =
            fs::read(&entry.wasm).map_err(|source| ManifestError::ReadWasm { path: entry.wasm.clone(), source })?;
        let config = match entry.config.as_ref() {
            Some(path) => fs::read(path).map_err(|source| ManifestError::ReadConfig { path: path.clone(), source })?,
            None => Vec::new(),
        };
        components.push(PackedComponent {
            wasm,
            config,
            name: entry.name,
            export: entry.export,
            replicas: entry.replicas,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_json_round_trips() {
        // The schema the hub's `spawn_substrate` boot-manifest injection writes
        // with `serde_json::json!` — field names here are the contract the hub
        // side mirrors.
        let json = r#"{
            "chassis": "headless",
            "tick_hz": 30,
            "components": [
                {"wasm": "/abs/a.wasm", "config": "/abs/a.cfg", "name": "a", "replicas": 3},
                {"wasm": "/abs/b.wasm"}
            ]
        }"#;
        let manifest: BootManifest = serde_json::from_str(json).expect("parse manifest");
        assert_eq!(manifest.chassis.as_deref(), Some("headless"));
        assert_eq!(manifest.title, None);
        assert_eq!(manifest.tick_hz, Some(30));
        assert_eq!(manifest.components.len(), 2);
        assert_eq!(manifest.components[0].name.as_deref(), Some("a"));
        assert_eq!(manifest.components[0].replicas, Some(3));
        assert_eq!(manifest.components[1].config, None);
        // Absent `replicas` defaults to `None` — today's one-instance
        // behaviour for every manifest written before this field existed.
        assert_eq!(manifest.components[1].replicas, None);
    }

    /// A per-test scratch directory under the system temp dir, unique
    /// per call so concurrent test threads never collide.
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::env;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-boot-manifest-{tag}-{}-{seq}", process::id()));
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
                {"wasm": wasm_a, "config": cfg_a, "name": "first", "replicas": 4},
                {"wasm": wasm_b, "export": "alt"},
            ],
        });
        fs::write(&manifest_path, serde_json::to_vec(&manifest_json).expect("serialize manifest"))
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
                    replicas: Some(4),
                },
                PackedComponent {
                    wasm: vec![0xfe, 0xff],
                    config: Vec::new(),
                    name: None,
                    export: Some("alt".to_owned()),
                    replicas: None,
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
        fs::write(&manifest_path, serde_json::to_vec(&manifest_json).expect("serialize manifest"))
            .expect("write manifest");

        let err = pack_from_manifest(&manifest_path).expect_err("missing wasm errors");
        assert!(matches!(err, ManifestError::ReadWasm { .. }), "{err:?}");

        fs::remove_dir_all(&dir).ok();
    }
}
