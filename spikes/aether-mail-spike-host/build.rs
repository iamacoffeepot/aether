// Builds the wasm guest crate for wasm32-unknown-unknown into a private
// target directory under OUT_DIR, then copies the .wasm into OUT_DIR/guest.wasm
// so main.rs can include_bytes! it. A separate target dir avoids the cargo
// build-lock collision that occurs when an inner cargo invocation tries to
// share the outer build's target directory.
//
// The guest is built with the same profile as the host: PROFILE=release in
// build.rs implies `cargo build --release` for the guest. This keeps debug
// host runs honest (matching debug guest) and release runs honest (matching
// release guest) — important because bench numbers from a release host
// against a release guest are the only ones that mean anything.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    // spikes/aether-mail-spike-host → spikes/aether-mail-spike-guest
    let guest_dir = manifest_dir
        .parent()
        .unwrap()
        .join("aether-mail-spike-guest");

    println!("cargo:rerun-if-changed={}", guest_dir.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        guest_dir.join("Cargo.toml").display()
    );

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let guest_target = out_dir.join("guest-target");

    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let release = profile == "release";

    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    cmd.args(["--target", "wasm32-unknown-unknown", "--manifest-path"])
        .arg(guest_dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&guest_target);

    let status = cmd.status().expect("failed to invoke cargo to build guest");
    assert!(status.success(), "guest wasm build failed");

    let built = guest_target
        .join("wasm32-unknown-unknown")
        .join(&profile)
        .join("aether_mail_spike_guest.wasm");
    let dest = out_dir.join("guest.wasm");
    std::fs::copy(&built, &dest).expect("failed to copy guest.wasm into OUT_DIR");
}
