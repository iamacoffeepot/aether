//! ADR-0090 c1 (issue 1256) integration coverage for the typed
//! `FfiActor::Config` path. Loads the `probe_with_config` example
//! cdylib through a [`TestBench`] and asserts the wasm guest's
//! `init_v2_p32` decode-error surfaces in `LoadResult::Err` when the
//! load mail carries no config bytes — c1 wires the host probe and
//! the guest shim but does not yet thread real config bytes through
//! the load mail (that is c2). The negative path is the load-bearing
//! evidence here: a typed-config guest reaching a substrate that
//! still passes `&[]` MUST fail loudly with a decode error, not load
//! silently with garbage state.
//!
//! Once c2 lands and the load mail carries `config_bytes`, the
//! companion positive test (currently skipped under
//! `AETHER_CONFIG_C2`) flips on.

#![allow(clippy::print_stderr)]

use std::path::Path;

use aether_actor::Actor;
use aether_capabilities::ComponentHostCapability;
use aether_kinds::{LoadComponent, LoadResult};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};
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
