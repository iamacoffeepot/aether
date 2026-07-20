//! ADR-0090 c1 (issue 1256) integration coverage for the typed
//! `WasmActor::Config` path. Loads the `ProbeWithConfig` actor from the
//! `probe` bundle (issue 1994, ADR-0096) via `export: Some("test.probe_with_config")`
//! through a [`SubstrateBench`] and asserts the wasm guest's config init path
//! handles both empty and explicit config bytes. Issue 2878 changed the empty
//! path from "decode error" to "boot from `Config::default()`"; the encoded
//! config path still proves the load mail's `config` bytes reach
//! `Component::instantiate` and the guest's init shim.

#![allow(clippy::print_stderr)]

use std::path::Path;

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_data::Kind;
use aether_kinds::{LoadComponent, LoadResult};
use aether_substrate_bundle::substrate_bench::{BenchOp, SubstrateBench, test_helpers::require_runtime};
use aether_test_fixtures_kinds::{ConfigEcho, ConfigQuery, ProbeConfig};
use std::fs;

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

/// Issue 2878: an empty config byte slice resolves guest-side to
/// `ProbeConfig::default()` instead of failing decode.
#[test]
fn typed_config_guest_without_config_bytes_uses_default() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut bench = SubstrateBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read::<&Path>(wasm_path.as_ref()).expect("read fixture wasm");

    let report = bench
        .execute(vec![
            (
                "load",
                BenchOp::send_and_await(
                    ComponentHostCapability::NAMESPACE,
                    &LoadComponent {
                        wasm,
                        name: Some("probe_with_config".to_owned()),
                        config: Vec::new(),
                        export: Some("test.probe_with_config".to_owned()),
                    },
                ),
            ),
            (
                "echo",
                BenchOp::send_and_await(
                    format!("aether.component/{}:probe_with_config", aether_component::WasmTrampoline::NAMESPACE),
                    &ConfigQuery,
                ),
            ),
        ])
        .expect("load sequence");

    match report.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { capabilities, .. } => {
            let cfg = capabilities.config.expect("typed-config component advertises its config kind");
            assert_eq!(cfg.id, <ProbeConfig as Kind>::ID);
            assert_eq!(cfg.name, <ProbeConfig as Kind>::NAME);
        }
        LoadResult::Err { error } => {
            panic!("typed-config guest without config bytes failed to load: {error}")
        }
    }

    let echo = report.reply::<ConfigEcho>("echo").expect("decode ConfigEcho");
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
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut bench = SubstrateBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read::<&Path>(wasm_path.as_ref()).expect("read fixture wasm");

    let config = ProbeConfig { seed: 0xABCD_1234, label: "c2-round-trip".to_owned() };
    let config_bytes = config.encode_into_bytes();

    let report = bench
        .execute(vec![
            (
                "load",
                BenchOp::send_and_await(
                    ComponentHostCapability::NAMESPACE,
                    &LoadComponent {
                        wasm,
                        name: Some("probe_with_config".to_owned()),
                        config: config_bytes,
                        export: Some("test.probe_with_config".to_owned()),
                    },
                ),
            ),
            (
                "echo",
                BenchOp::send_and_await(
                    format!("aether.component/{}:probe_with_config", aether_component::WasmTrampoline::NAMESPACE),
                    &ConfigQuery,
                ),
            ),
        ])
        .expect("load + query sequence");

    match report.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { capabilities, .. } => {
            let cfg = capabilities.config.expect("typed-config component advertises its config kind");
            assert_eq!(cfg.id, <ProbeConfig as Kind>::ID);
            assert_eq!(cfg.name, <ProbeConfig as Kind>::NAME);
        }
        LoadResult::Err { error } => {
            panic!("typed-config guest with config bytes failed to load: {error}")
        }
    }

    let echo = report.reply::<ConfigEcho>("echo").expect("decode ConfigEcho");
    assert_eq!(echo.seed, 0xABCD_1234, "seed round-trips through init");
    assert_eq!(echo.label, "c2-round-trip", "label round-trips through init");
}
