//! Stage the bundle pack the generic bundle bins embed
//! (iamacoffeepot/aether#1529, generalizing the single-wasm #1518 stage).
//!
//! `aether-bundle-desktop` / `aether-bundle-headless` do
//! `include_bytes!(concat!(env!("OUT_DIR"), "/bundle_pack.bin"))`, so a
//! pack blob must exist in `OUT_DIR` at compile time. `cargo xtask
//! bundle` builds the listed components for `wasm32-unknown-unknown`,
//! writes a JSON [`BundleManifest`], and points `AETHER_BUNDLE_MANIFEST`
//! at it; this reads the manifest, packs the wasm + config files into
//! one indexed blob (see `src/bundle_pack.rs`, compiled in via a
//! `#[path]` module below so the encoder and the bins' decoder are the
//! same code), and emits it. A
//! normal workspace build (no env set) writes an empty-pack placeholder
//! so the bins still compile — they just boot componentless if run,
//! which only the bundle flow ever does for real.

use std::{env, fs, path::Path, path::PathBuf};

// The pack encoder + manifest schema, shared with the lib (where the
// bundle bins decode). Self-contained std+serde, so the same file
// compiles in both contexts; `dead_code` because the build script only
// uses the encode half.
#[allow(dead_code)]
#[path = "src/bundle_pack.rs"]
mod bundle_pack;

use bundle_pack::{BundleManifest, ChassisSettings, Pack, PackedComponent, encode_pack};

fn main() {
    println!("cargo:rerun-if-env-changed=AETHER_BUNDLE_MANIFEST");
    println!("cargo:rerun-if-changed=src/bundle_pack.rs");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"))
        .join("bundle_pack.bin");
    let pack = env::var_os("AETHER_BUNDLE_MANIFEST").map_or_else(Pack::default, |manifest_path| {
        pack_from_manifest(Path::new(&manifest_path))
    });
    fs::write(&out, encode_pack(&pack)).expect("write bundle pack blob");
}

/// Read the manifest plus every wasm / config file it names into a
/// [`Pack`], registering each input for rerun-if-changed.
fn pack_from_manifest(manifest_path: &Path) -> Pack {
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    let manifest_json = fs::read_to_string(manifest_path)
        .unwrap_or_else(|e| panic!("read bundle manifest from {}: {e}", manifest_path.display()));
    let manifest: BundleManifest = serde_json::from_str(&manifest_json)
        .unwrap_or_else(|e| panic!("parse bundle manifest at {}: {e}", manifest_path.display()));
    let mut components = Vec::new();
    for entry in manifest.components {
        println!("cargo:rerun-if-changed={}", entry.wasm.display());
        let wasm = fs::read(&entry.wasm)
            .unwrap_or_else(|e| panic!("read component wasm from {}: {e}", entry.wasm.display()));
        let config = entry.config.as_ref().map_or_else(Vec::new, |path| {
            println!("cargo:rerun-if-changed={}", path.display());
            fs::read(path)
                .unwrap_or_else(|e| panic!("read component config from {}: {e}", path.display()))
        });
        components.push(PackedComponent {
            wasm,
            config,
            name: entry.name,
            export: entry.export,
        });
    }
    Pack {
        chassis: ChassisSettings {
            title: manifest.title,
            window_mode: manifest.window_mode,
            tick_hz: manifest.tick_hz,
        },
        components,
    }
}
