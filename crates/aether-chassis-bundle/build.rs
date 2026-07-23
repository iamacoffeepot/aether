//! Stage the package artifact the standalone bundle bins embed (ADR-0163 §1,
//! issue #3972 — retiring the #1529 inline-bytes pack blob).
//!
//! `aether-bundle-desktop` / `aether-bundle-headless` `include!` a generated
//! `OUT_DIR/embedded_pack.rs` that names the persisted `pack/manifest` bytes
//! and each `pack/objects/<sha256>` object via `include_bytes!`. `cargo xtask
//! bundle` builds the listed components, emits the depot-shaped pack
//! (`pack/manifest` + content-addressed objects) into a build directory, and
//! points `AETHER_BUNDLE_PACK` at that directory's root; this build script
//! copies the pack into `OUT_DIR` and generates the include source, so the
//! bins boot through `aether_chassis::package`'s `decode_manifest` +
//! object-resolution path (the same one the depot boot uses). A plain
//! workspace build (no env) writes an empty manifest and no objects, which the
//! bins read as "no package — boot componentless".
//!
//! This build script owns no encoding: it copies bytes the depot tooling
//! already produced and emits `include_bytes!` references, so the manifest
//! byte format lives in exactly one place (`aether_chassis::package`).

use std::fmt::Write as _;
use std::{env, fs, path::PathBuf};

// Build scripts communicate with cargo through env only (OUT_DIR + the pack
// root) — there is no config layer at build time.
#[allow(clippy::disallowed_methods)]
fn main() {
    println!("cargo:rerun-if-env-changed=AETHER_BUNDLE_PACK");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));
    let embed_objects = out_dir.join("pack").join("objects");
    fs::create_dir_all(&embed_objects).expect("create OUT_DIR/pack/objects");
    let embed_manifest = out_dir.join("pack").join("manifest");

    let mut object_names: Vec<String> = Vec::new();

    if let Some(pack_root) = env::var_os("AETHER_BUNDLE_PACK") {
        let pack_dir = PathBuf::from(&pack_root).join("pack");
        let src_manifest = pack_dir.join("manifest");
        let src_objects = pack_dir.join("objects");
        println!("cargo:rerun-if-changed={}", src_manifest.display());
        println!("cargo:rerun-if-changed={}", src_objects.display());
        fs::copy(&src_manifest, &embed_manifest)
            .unwrap_or_else(|e| panic!("copy embedded pack manifest from {}: {e}", src_manifest.display()));
        for entry in fs::read_dir(&src_objects).unwrap_or_else(|e| panic!("read {}: {e}", src_objects.display())) {
            let entry = entry.expect("read pack/objects entry");
            let name = entry.file_name().into_string().expect("pack object filename is UTF-8 hex");
            println!("cargo:rerun-if-changed={}", entry.path().display());
            fs::copy(entry.path(), embed_objects.join(&name))
                .unwrap_or_else(|e| panic!("copy pack object {name}: {e}"));
            object_names.push(name);
        }
    } else {
        // No pack staged (a plain build): an empty manifest embeds as zero
        // bytes, which the bin reads as "no package — boot componentless". No
        // format knowledge is duplicated here to synthesize it.
        fs::write(&embed_manifest, []).expect("write empty embedded pack manifest");
    }

    // Stable object order so the generated source is deterministic.
    object_names.sort();

    // Generate the include source: the manifest bytes plus each object keyed by
    // its hex filename, all `include_bytes!` against this same OUT_DIR (shared
    // between this build script and the bins it compiles).
    let mut src = String::new();
    src.push_str("pub const MANIFEST: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/pack/manifest\"));\n");
    src.push_str("pub const OBJECTS: &[(&str, &[u8])] = &[\n");
    for name in &object_names {
        writeln!(src, "    ({name:?}, include_bytes!(concat!(env!(\"OUT_DIR\"), \"/pack/objects/{name}\"))),")
            .expect("write embedded object entry");
    }
    src.push_str("];\n");
    fs::write(out_dir.join("embedded_pack.rs"), src).expect("write embedded_pack.rs");
}
