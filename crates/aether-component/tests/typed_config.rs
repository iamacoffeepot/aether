//! ADR-0090 c1 (issue 1256) integration coverage for the typed
//! `WasmActor::Config` path. Loads the `ProbeWithConfig` actor from the
//! `probe` bundle (issue 1994, ADR-0096) via `export: Some("test.probe_with_config")`
//! through a [`SubstrateHarness`] and asserts the wasm guest's config init path
//! handles both empty and explicit config bytes. Issue 2878 changed the empty
//! path from "decode error" to "boot from `Config::default()`"; the encoded
//! config path still proves the load mail's `config` bytes reach
//! `Component::instantiate` and the guest's init shim.

use std::path::Path;

use aether_actor::ActorRef;
use aether_component::ComponentHostCapability;
use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DescribeComponent, DescribeComponentResult, LoadComponent};
use aether_substrate::testing::successor_wasm;
use aether_test_fixtures_bundle::ProbeWithConfig;
use aether_test_fixtures_kinds::{ConfigEcho, ConfigQuery, ProbeConfig};
use std::fs;

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

/// Load `probe_with_config` with `config` bytes, assert it advertises its
/// config kind, and hand back the loaded guest's reference.
fn load_probe(harness: &mut SubstrateHarness, wasm_path: &Path, config: Vec<u8>) -> ActorRef<ProbeWithConfig> {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let load = LoadComponent { wasm, name: None, config, export: None };
    let probe = harness
        .load::<ProbeWithConfig>(load)
        .unwrap_or_else(|error| panic!("the typed-config guest failed to load: {error}"));
    let path = harness.actor_path(&probe);

    let host = harness.actor_ref::<ComponentHostCapability>();
    let described = harness
        .execute(vec![(
            "describe",
            HarnessOp::send_and_await_reply(&host, &DescribeComponent { name: path.to_string() }),
        )])
        .expect("describe sequence")
        .reply::<DescribeComponentResult>("describe")
        .expect("decode DescribeComponentResult");
    let DescribeComponentResult::Ok { capabilities } = described else {
        panic!("the loaded guest is described: {described:?}");
    };
    let cfg = capabilities.config.expect("typed-config component advertises its config kind");
    assert_eq!(cfg.id, <ProbeConfig as Kind>::ID);
    assert_eq!(cfg.name, <ProbeConfig as Kind>::NAME);

    probe
}

/// Ask the loaded `probe_with_config` guest which config its `init` saw.
fn echo_config(harness: &mut SubstrateHarness, probe: ActorRef<ProbeWithConfig>) -> ConfigEcho {
    harness
        .execute(vec![("echo", HarnessOp::send_and_await_reply(&probe, &ConfigQuery))])
        .expect("echo sequence")
        .reply::<ConfigEcho>("echo")
        .expect("decode ConfigEcho")
}

/// Issue 2878: an empty config byte slice resolves guest-side to
/// `ProbeConfig::default()` instead of failing decode.
#[test]
fn typed_config_guest_without_config_bytes_uses_default() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let probe = load_probe(&mut harness, &wasm_path, Vec::new());

    let echo = echo_config(&mut harness, probe);
    let expected = ProbeConfig::default();
    assert_eq!(echo.seed, expected.seed, "default seed reaches init");
    assert_eq!(echo.label, expected.label, "default label reaches init");
}

/// ADR-0090 c2 (issue 1257) positive path: load the typed-config
/// fixture WITH real `ProbeConfig` bytes on the load mail, then query
/// it — the `ConfigEcho` reply must echo the exact `(seed, label)` the
/// guest decoded at `init`. This proves the full c2 delivery seam: the
/// load mail's `config` bytes reach `Component::instantiate`, the c1
/// ABI writes them into the guest's linear memory, and `init_with_config_p32`
/// decodes them into `Probe::init(config, ctx)`.
///
/// c1 parked this behind `AETHER_CONFIG_C2` because the delivery seam
/// hardcoded `&[]`; c2 wires it, so the test runs unconditionally now.
#[test]
fn typed_config_guest_with_config_bytes_round_trips() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let config = ProbeConfig { seed: 0xABCD_1234, label: "c2-round-trip".to_owned() };
    let probe = load_probe(&mut harness, &wasm_path, config.encode_into_bytes());

    let echo = echo_config(&mut harness, probe);
    assert_eq!(echo.seed, 0xABCD_1234, "seed round-trips through init");
    assert_eq!(echo.label, "c2-round-trip", "label round-trips through init");
}

/// ADR-0241 §7: a replace that supplies no config builds its candidate from
/// the config the guest was spawned with.
#[test]
fn a_replace_without_config_reuses_the_spawn_config() {
    // Catches: the trampoline forgets its spawn config, so a replace with no
    // config hands the typed candidate empty bytes and it boots from
    // `ProbeConfig::default()`.
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let config = ProbeConfig { seed: 0x7085_0001, label: "spawn-config".to_owned() };
    let probe = load_probe(&mut harness, &wasm_path, config.encode_into_bytes());

    // A successor build of the same code: identical bytes would answer with
    // no swap, and no candidate would be built.
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    if let Err(error) = harness.publish(successor_wasm(&wasm, 1)) {
        panic!("the republish commits: {error}");
    }

    assert_eq!(
        echo_config(&mut harness, probe),
        ConfigEcho { seed: config.seed, label: config.label },
        "the candidate's init sees the spawn config"
    );
}
