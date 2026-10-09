//! The demo takes `aether-kit` under `library` for its actors' kinds, so its
//! wasm must carry none of the kit's embedded assets. The bug this catches:
//! the kit's `export_asset!` gate removed or inverted, after which every
//! module that links the kit as a library ships the kit's tile again.
//!
//! Reads the prebuilt demo wasm the way `aether-actor`'s `asset_sections`
//! test reads its fixture: skips when it isn't built, and fails under
//! `AETHER_REQUIRE_RUNTIME=1`.

use std::path::PathBuf;
use std::{env, fs};

use wasmparser::{Parser, Payload};

// Test-only: CARGO_TARGET_DIR is the standard cargo build-output override, not
// cap config — honor it so wasm built into an out-of-tree target dir is found.
#[allow(clippy::disallowed_methods)]
fn locate_demo_wasm() -> Option<PathBuf> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?.to_path_buf();
    let target_root = env::var_os("CARGO_TARGET_DIR").map_or_else(|| workspace.join("target"), PathBuf::from);
    ["debug", "release"]
        .into_iter()
        .map(|profile| target_root.join("wasm32-unknown-unknown").join(profile).join("aether_demo.wasm"))
        .find(|candidate| candidate.exists())
}

// Test-only skip diagnostic, visible alongside `test ... ok` lines.
#[allow(clippy::print_stderr)]
#[test]
fn demo_wasm_carries_no_kit_tile() {
    // Test-binary runtime probe, not cap config.
    #[allow(clippy::disallowed_methods)]
    let require = env::var_os("AETHER_REQUIRE_RUNTIME").is_some();
    let Some(wasm_path) = locate_demo_wasm() else {
        assert!(!require, "AETHER_REQUIRE_RUNTIME=1 but aether_demo wasm not pre-built");
        eprintln!("skipping: aether_demo wasm not built under target/wasm32-unknown-unknown/{{debug,release}}");
        return;
    };
    let bytes = fs::read(&wasm_path).expect("read aether_demo wasm");

    let assets: Vec<String> = Parser::new(0)
        .parse_all(&bytes)
        .filter_map(|payload| match payload.expect("parse aether_demo.wasm") {
            Payload::CustomSection(reader) => Some(reader.name().to_owned()),
            _ => None,
        })
        .filter(|name| name.starts_with("aether.asset."))
        .collect();

    assert!(assets.is_empty(), "the demo links the kit under `library` and must carry no asset, found {assets:?}");
}
