//! The checked-in boot manifest boots on the real headless chassis.
//!
//! Reads `demo.boot.json`, points each entry's `wasm` at the pre-built
//! artifact of the same file stem and its `config_json` at the workspace root
//! (a boot manifest resolves paths against the working directory, which the
//! dev run sets to the repository root), and boots `HeadlessChassis` on it
//! through the same argv resolution the binary runs. `build` returns only once
//! every boot entry has answered its load `Ok`, so a stale export name or wasm
//! stem, an order the dependency refusals reject, or a `controller.json` that no
//! longer encodes against the controller's schema fails the build here.
//!
//! What this cannot see is the demo's mail: the chassis hands a boot-loaded
//! component back only as a position, never as a proven reference.
//! `tests/scenario.rs` covers the load reaching the viewer.

use std::fs;
use std::path::{Path, PathBuf};

use aether_actor::Addressable;
use aether_chassis::boot::CommonEnv;
use aether_chassis_headless::{HeadlessChassis, HeadlessCli};
use aether_data::ActorPath;
use aether_demo::Demo;
use aether_harness_substrate::test_helpers::{init_save_sandbox, require_wasm};
use aether_substrate::Chassis as _;
use clap::Parser;
use serde_json::Value;

/// The repository root: this crate lives at `crates/aether-demo`.
fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).expect("workspace root")
}

/// `demo.boot.json` with each `wasm` replaced by the located artifact of
/// its file stem and each `config_json` anchored at the workspace root.
fn staged_manifest(sandbox: &Path) -> Option<PathBuf> {
    let checked_in =
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("demo.boot.json")).expect("read demo.boot.json");
    let mut manifest: Value = serde_json::from_slice(&checked_in).expect("parse demo.boot.json");

    for entry in manifest["components"].as_array_mut().expect("a components array") {
        let stem = Path::new(entry["wasm"].as_str().expect("a wasm path"))
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("a wasm file stem")
            .to_owned();
        entry["wasm"] = Value::from(require_wasm(&stem)?.to_str().expect("a UTF-8 wasm path"));
        if let Some(config) = entry.get("config_json").and_then(Value::as_str) {
            entry["config_json"] = Value::from(workspace_root().join(config).to_str().expect("a UTF-8 config path"));
        }
    }

    let staged = sandbox.join("demo.boot.json");
    fs::write(&staged, serde_json::to_vec(&manifest).expect("serialize the staged manifest"))
        .expect("write the staged manifest");
    Some(staged)
}

#[test]
fn checked_in_boot_manifest_boots_the_demo() {
    let sandbox = init_save_sandbox("demo-boot");
    let Some(manifest) = staged_manifest(sandbox) else {
        return;
    };
    let assets = workspace_root().join("crates/aether-mesh/examples");
    let (save, config) = (sandbox.join("save"), sandbox.join("config"));

    let cli = HeadlessCli::try_parse_from([
        Path::new("aether-headless"),
        Path::new("--boot-manifest"),
        &manifest,
        Path::new("--assets-dir"),
        &assets,
        Path::new("--save-dir"),
        &save,
        Path::new("--config-dir"),
        &config,
    ])
    .expect("parse the headless argv");
    let env = CommonEnv::resolve(cli).expect("resolve the headless env");
    let built = HeadlessChassis::build(env).expect("every boot entry in demo.boot.json loads");

    let demo = ActorPath::new(&format!("aether.component/aether.embedded:{}", Demo::NAMESPACE))
        .expect("a well-formed actor path");
    let resolved = built.resolve_address(&demo);
    assert!(resolved.is_ok(), "the demo component {demo} is not live when build returns: {resolved:?}");
}
