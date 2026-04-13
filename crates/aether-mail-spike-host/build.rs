// Builds the wasm guest crate for wasm32-unknown-unknown into a private
// target directory under OUT_DIR, then copies the .wasm into OUT_DIR/guest.wasm
// so main.rs can include_bytes! it. A separate target dir avoids the cargo
// build-lock collision that occurs when an inner cargo invocation tries to
// share the outer build's target directory.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let guest_dir = workspace_root
        .join("crates")
        .join("aether-mail-spike-guest");

    println!("cargo:rerun-if-changed={}", guest_dir.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        guest_dir.join("Cargo.toml").display()
    );

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let guest_target = out_dir.join("guest-target");

    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--manifest-path",
        ])
        .arg(guest_dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&guest_target)
        .status()
        .expect("failed to invoke cargo to build guest");

    assert!(status.success(), "guest wasm build failed");

    let built = guest_target
        .join("wasm32-unknown-unknown")
        .join("release")
        .join("aether_mail_spike_guest.wasm");
    let dest = out_dir.join("guest.wasm");
    std::fs::copy(&built, &dest).expect("failed to copy guest.wasm into OUT_DIR");
}
