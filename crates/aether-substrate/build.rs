// Builds aether-hello-component for wasm32-unknown-unknown into a private
// target directory under OUT_DIR, then copies the resulting `.wasm` into
// `OUT_DIR/hello_component.wasm` so `main.rs` can `include_bytes!` it.
//
// A separate target dir avoids the cargo build-lock collision that occurs
// when an inner cargo invocation shares the outer build's target dir.
//
// The guest is built with the same profile as the host — release host →
// release guest, debug host → debug guest. Milestone 1 runs at any
// profile; the shape matches the spike crate for consistency.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let guest_dir = workspace_root.join("crates").join("aether-hello-component");

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
    assert!(status.success(), "hello-component wasm build failed");

    let built = guest_target
        .join("wasm32-unknown-unknown")
        .join(&profile)
        .join("aether_hello_component.wasm");
    let dest = out_dir.join("hello_component.wasm");
    std::fs::copy(&built, &dest).expect("failed to copy hello_component.wasm into OUT_DIR");
}
