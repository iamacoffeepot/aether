//! `--describe` manifest smoke test (ADR-0115, issue 1953): run the real
//! `aether-headless` binary with `--describe`, parse the JSON it
//! prints, and assert the chassis kind plus a non-empty linked-cap list.
//! This is the same `--describe` mode the hub's binary store forks once at
//! upload time to capture what a stored binary is — the cap test (the
//! `FleetHarness` scenario) exercises that fork path end to end; this one
//! pins the binary's own contract.

use std::process::Command;

use aether_kinds::BinaryManifest;

/// `aether-headless --describe` prints a `BinaryManifest` JSON
/// reporting `chassis == "headless"`, a non-empty cap list including the
/// fs cap, and non-empty build provenance, then exits 0.
#[test]
fn headless_describe_emits_manifest() {
    let bin = env!("CARGO_BIN_EXE_aether-headless");
    let output =
        Command::new(bin).arg("--describe").output().expect("test setup: running the headless binary with --describe");
    assert!(output.status.success(), "--describe should exit 0; stderr: {}", String::from_utf8_lossy(&output.stderr));

    let manifest: BinaryManifest =
        serde_json::from_slice(&output.stdout).expect("test setup: --describe stdout is a BinaryManifest JSON");
    assert_eq!(manifest.chassis, "headless", "reports the headless profile");
    assert!(!manifest.caps.is_empty(), "the headless chassis links a non-empty cap set");
    // ADR-0155: the roster is claim-derived, so these assert the real claim
    // path — not a hand list. `aether.fs` is a `with_full_stack_caps` cap,
    // `aether.audio` is the headless inline fail-fast sink, and
    // `aether.rpc.server` / `aether.http.server` are the always-claim servers
    // (ADR-0155 §3) the old hand list silently omitted.
    for expected in ["aether.fs", "aether.audio", "aether.rpc.server", "aether.http.server"] {
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

    // ADR-0162: the manifest self-reports its config surface, and — like the
    // cap roster — that surface is derived from the real headless composition,
    // never a hand list. These assertions pin that it is *this* profile's
    // surface, not a fleet-wide union:
    //   - the headless timer driver composes the tick knob, so `AETHER_TICK_HZ`
    //     / `--tick-hz` are present, as are the always-composed RPC-port knobs;
    //   - the window / wireframe knobs live on the *desktop* driver, so a
    //     composition-derived headless surface must exclude them. A regression
    //     that reverted to a hand-maintained union (the ADR-0162 drift class)
    //     would wrongly leak `AETHER_WINDOW_MODE` / `--window-mode` in here.
    assert!(
        manifest.env_keys.contains(&"AETHER_TICK_HZ".to_owned()),
        "headless composes the tick knob: {:?}",
        manifest.env_keys
    );
    assert!(
        manifest.env_keys.contains(&"AETHER_RPC_PORT".to_owned()),
        "headless composes the RPC server: {:?}",
        manifest.env_keys
    );
    assert!(
        !manifest.env_keys.contains(&"AETHER_WINDOW_MODE".to_owned()),
        "the window knob is desktop-only; a composition-derived headless surface excludes it: {:?}",
        manifest.env_keys
    );
    assert!(manifest.env_keys.windows(2).all(|w| w[0] <= w[1]), "env_keys are sorted, got {:?}", manifest.env_keys);

    assert!(
        manifest.argv_flags.contains(&"tick-hz".to_owned()),
        "headless argv surface carries the tick flag: {:?}",
        manifest.argv_flags
    );
    assert!(
        manifest.argv_flags.contains(&"rpc-port".to_owned()),
        "headless argv surface carries the RPC-port flag: {:?}",
        manifest.argv_flags
    );
    assert!(
        !manifest.argv_flags.contains(&"window-mode".to_owned()),
        "the window flag is desktop-only; the headless argv surface excludes it: {:?}",
        manifest.argv_flags
    );
    assert!(
        manifest.argv_flags.windows(2).all(|w| w[0] <= w[1]),
        "argv_flags are sorted, got {:?}",
        manifest.argv_flags
    );
}

/// ADR-0162 backward compatibility: a `BinaryManifest` JSON captured before the
/// config-surface fields existed (an old content hash's stored sidecar) still
/// parses — `env_keys` / `argv_flags` are `#[serde(default)]`, so an old-shape
/// manifest reads back with empty sets rather than failing the store. This is
/// the read policy stated in ADR-0162; strict-required rejection of a
/// nonconforming *new* upload is the upload gate's job (#3936), not a
/// retroactive parse failure.
#[test]
fn pre_config_surface_manifest_json_still_parses() {
    let legacy = r#"{
        "chassis": "headless",
        "caps": ["aether.fs"],
        "git_sha": "deadbee",
        "profile": "debug",
        "target": "x86_64-unknown-linux-gnu"
    }"#;
    let manifest: BinaryManifest =
        serde_json::from_str(legacy).expect("an old-shape manifest lacking the config-surface fields still parses");
    assert_eq!(manifest.chassis, "headless");
    assert!(manifest.env_keys.is_empty(), "an absent env_keys field defaults to empty");
    assert!(manifest.argv_flags.is_empty(), "an absent argv_flags field defaults to empty");
}
