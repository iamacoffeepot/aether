//! Stage the wasm that the `aether-game` bin embeds (#1518).
//!
//! `aether-game` does `include_bytes!(concat!(env!("OUT_DIR"), "/game.wasm"))`,
//! so a `game.wasm` must exist in `OUT_DIR` at compile time. `cargo xtask
//! bundle` builds the game component for `wasm32-unknown-unknown` and points
//! `AETHER_GAME_WASM` at the artifact; this copies it into place. A normal
//! workspace build (no env set) writes an empty placeholder so the bin still
//! compiles — it just boots an empty component if run, which only the bundle
//! flow ever does for real.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=AETHER_GAME_WASM");
    let out =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set by cargo")).join("game.wasm");
    match env::var_os("AETHER_GAME_WASM") {
        Some(src) => {
            let src = PathBuf::from(src);
            println!("cargo:rerun-if-changed={}", src.display());
            fs::copy(&src, &out)
                .unwrap_or_else(|e| panic!("copy game wasm from {}: {e}", src.display()));
        }
        None => fs::write(&out, []).expect("write empty game.wasm placeholder"),
    }
}
