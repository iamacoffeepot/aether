//! `--describe` manifest smoke test (ADR-0115, issue 1953): run the real
//! `aether-substrate-headless` binary with `--describe`, parse the JSON it
//! prints, and assert the chassis kind plus a non-empty linked-cap list.
//! This is the same `--describe` mode the hub's binary store forks once at
//! upload time to capture what a stored binary is — the cap test (the
//! `FleetBench` scenario) exercises that fork path end to end; this one
//! pins the binary's own contract.

use std::process::Command;

use aether_kinds::BinaryManifest;

/// `aether-substrate-headless --describe` prints a `BinaryManifest` JSON
/// reporting `chassis == "headless"`, a non-empty cap list including the
/// fs cap, and non-empty build provenance, then exits 0.
#[test]
fn headless_describe_emits_manifest() {
    let bin = env!("CARGO_BIN_EXE_aether-substrate-headless");
    let output = Command::new(bin)
        .arg("--describe")
        .output()
        .expect("test setup: running the headless binary with --describe");
    assert!(
        output.status.success(),
        "--describe should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let manifest: BinaryManifest = serde_json::from_slice(&output.stdout)
        .expect("test setup: --describe stdout is a BinaryManifest JSON");
    assert_eq!(manifest.chassis, "headless", "reports the headless profile");
    assert!(
        !manifest.caps.is_empty(),
        "the headless chassis links a non-empty cap set",
    );
    assert!(
        manifest.caps.iter().any(|c| c == "aether.fs"),
        "the headless chassis links the fs cap, got {:?}",
        manifest.caps,
    );
    // Build provenance is always baked (`unknown` fallbacks outside a git
    // checkout), so the fields are never empty.
    assert!(!manifest.git_sha.is_empty(), "git_sha is baked");
    assert!(!manifest.profile.is_empty(), "build profile is baked");
    assert!(!manifest.target.is_empty(), "target triple is baked");
}
