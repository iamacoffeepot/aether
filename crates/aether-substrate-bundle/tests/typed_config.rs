//! ADR-0090 c1 (issue 1256) integration coverage for the typed
//! `FfiActor::Config` path. Loads the `probe_with_config` example
//! cdylib through a [`TestBench`] and asserts the wasm guest's
//! `init_with_config_p32` decode-error surfaces in `LoadResult::Err` when the
//! load mail carries no config bytes — c1 wires the host probe and
//! the guest shim but does not yet thread real config bytes through
//! the load mail (that is c2). The negative path is the load-bearing
//! evidence here: a typed-config guest reaching a substrate that
//! still passes `&[]` MUST fail loudly with a decode error, not load
//! silently with garbage state.
//!
//! c2 (issue 1257) lands the delivery seam: the load mail now carries
//! `config` bytes threaded through `Component::instantiate`, so the
//! companion positive test (`..._with_config_bytes_round_trips`) runs
//! unconditionally — the parked `AETHER_CONFIG_C2` gate is retired.

#![allow(clippy::print_stderr)]

use std::path::Path;

use aether_actor::Actor;
use aether_capabilities::ComponentHostCapability;
use aether_data::Kind;
use aether_kinds::{LoadComponent, LoadResult};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};
use aether_test_fixtures::{ConfigEcho, ConfigQuery, ProbeConfig};
use std::fs;

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary.
#[allow(unused_imports)]
use aether_test_fixtures as _;

/// Until c2 threads config bytes through the load mail, a load against
/// a typed-config fixture (`Config = ProbeConfig`) MUST surface a
/// decode error rather than booting with default-initialised state.
/// This test pins that contract — a regression here means a host
/// running a typed-config guest could silently run with corrupt
/// config.
#[test]
fn typed_config_guest_without_config_bytes_surfaces_decode_error() {
    let Some(wasm_path) = require_runtime("probe_with_config") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read::<&Path>(wasm_path.as_ref()).expect("read fixture wasm");

    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: Some("probe_with_config".to_owned()),
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("load sequence");
    let result = loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult");

    match result {
        LoadResult::Err { error } => {
            assert!(
                error.contains("Config") || error.contains("decode") || error.contains("init"),
                "expected the LoadResult::Err message to mention the config decode failure; got: {error}",
            );
        }
        LoadResult::Ok { .. } => {
            panic!("typed-config guest loaded with no config bytes; expected a decode-error path")
        }
    }
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
    let Some(wasm_path) = require_runtime("probe_with_config") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read::<&Path>(wasm_path.as_ref()).expect("read fixture wasm");

    let config = ProbeConfig {
        seed: 0xABCD_1234,
        label: "c2-round-trip".to_owned(),
    };
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
                        export: None,
                    },
                ),
            ),
            (
                "echo",
                BenchOp::send_and_await(
                    format!(
                        "aether.component/{}:probe_with_config",
                        aether_capabilities::WasmTrampoline::NAMESPACE
                    ),
                    &ConfigQuery,
                ),
            ),
        ])
        .expect("load + query sequence");

    match report
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { capabilities, .. } => {
            let cfg = capabilities
                .config
                .expect("typed-config component advertises its config kind");
            assert_eq!(cfg.id, <ProbeConfig as Kind>::ID);
            assert_eq!(cfg.name, <ProbeConfig as Kind>::NAME);
        }
        LoadResult::Err { error } => {
            panic!("typed-config guest with config bytes failed to load: {error}")
        }
    }

    let echo = report
        .reply::<ConfigEcho>("echo")
        .expect("decode ConfigEcho");
    assert_eq!(echo.seed, 0xABCD_1234, "seed round-trips through init");
    assert_eq!(
        echo.label, "c2-round-trip",
        "label round-trips through init"
    );
}
