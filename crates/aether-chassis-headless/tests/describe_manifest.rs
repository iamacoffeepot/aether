//! `--describe` manifest smoke test (ADR-0115, issue 1953): run the real
//! `aether-substrate-headless` binary with `--describe`, parse the JSON it
//! prints, and assert the chassis kind plus a non-empty linked-cap list.
//! This is the same `--describe` mode the hub's binary store forks once at
//! upload time to capture what a stored binary is — the cap test (the
//! `FleetHarness` scenario) exercises that fork path end to end; this one
//! pins the binary's own contract.

use std::process::Command;

use aether_kinds::BinaryManifest;

/// `aether-substrate-headless --describe` prints a `BinaryManifest` JSON
/// reporting `chassis == "headless"`, a non-empty cap list including the
/// fs cap, and non-empty build provenance, then exits 0.
#[test]
fn headless_describe_emits_manifest() {
    let bin = env!("CARGO_BIN_EXE_aether-substrate-headless");
    let output =
        Command::new(bin).arg("--describe").output().expect("test setup: running the headless binary with --describe");
    assert!(output.status.success(), "--describe should exit 0; stderr: {}", String::from_utf8_lossy(&output.stderr));

    let manifest: BinaryManifest =
        serde_json::from_slice(&output.stdout).expect("test setup: --describe stdout is a BinaryManifest JSON");
    assert_eq!(manifest.chassis, "headless", "reports the headless profile");
    assert!(!manifest.caps.is_empty(), "the headless chassis links a non-empty cap set");
    // ADR-0155: the roster is claim-derived, so these assert the real claim
    // path — not a hand list. `aether.fs` is a `with_common_caps` cap;
    // `aether.game.gateway` pins that `with_common_caps` still composes the
    // (inert-by-default) game gateway, which the claim reserves even though no
    // player listener opens — catches someone dropping it from the common
    // chain. `aether.audio` is the headless inline fail-fast sink, and
    // `aether.rpc.server` / `aether.http.server` are the always-claim servers
    // (ADR-0155 §3) the old hand list silently omitted.
    for expected in ["aether.fs", "aether.game.gateway", "aether.audio", "aether.rpc.server", "aether.http.server"] {
        assert!(
            manifest.caps.iter().any(|c| c == expected),
            "claim-derived roster must include {expected}, got {:?}",
            manifest.caps
        );
    }
    // BTreeSet order: the manifest caps are sorted (ADR-0155).
    assert!(manifest.caps.windows(2).all(|w| w[0] <= w[1]), "claim-derived caps are sorted, got {:?}", manifest.caps);
    // Build provenance is always baked (`unknown` fallbacks outside a git
    // checkout), so the fields are never empty.
    assert!(!manifest.git_sha.is_empty(), "git_sha is baked");
    assert!(!manifest.profile.is_empty(), "build profile is baked");
    assert!(!manifest.target.is_empty(), "target triple is baked");
}
