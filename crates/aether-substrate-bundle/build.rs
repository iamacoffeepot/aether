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

// The pack encoder + manifest schema + reader, shared with the lib
// (where the bundle bins decode and the chassis runtime reads
// `AETHER_BOOT_MANIFEST`). Self-contained std+serde, so the same file
// compiles in both contexts; `dead_code` because the build script
// only exercises the encode + read-manifest half.
#[allow(dead_code)]
#[path = "src/bundle_pack.rs"]
mod bundle_pack;

use bundle_pack::{Pack, encode_pack, pack_from_manifest, read_manifest};

fn main() {
    println!("cargo:rerun-if-env-changed=AETHER_BUNDLE_MANIFEST");
    println!("cargo:rerun-if-changed=src/bundle_pack.rs");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"))
        .join("bundle_pack.bin");
    let pack = env::var_os("AETHER_BUNDLE_MANIFEST").map_or_else(Pack::default, |manifest_path| {
        pack_for(Path::new(&manifest_path))
    });
    fs::write(&out, encode_pack(&pack)).expect("write bundle pack blob");
}

/// Register the manifest plus every wasm / config it names for
/// rerun-if-changed, then read them into a [`Pack`] via the shared
/// [`pack_from_manifest`] reader (the runtime boot path reuses the same
/// reader, so the encode and read logic live in one place).
fn pack_for(manifest_path: &Path) -> Pack {
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    let manifest = read_manifest(manifest_path).unwrap_or_else(|e| panic!("{e}"));
    for entry in &manifest.components {
        println!("cargo:rerun-if-changed={}", entry.wasm.display());
        if let Some(config) = &entry.config {
            println!("cargo:rerun-if-changed={}", config.display());
        }
    }
    pack_from_manifest(manifest_path).unwrap_or_else(|e| panic!("{e}"))
}
