//! Headless boot-time autoload smoke (iamacoffeepot/aether#1529).
//!
//! Boots a real `HeadlessChassis` (not the test bench — the point is
//! the headless `build_inner` autoload drain) with a probe component
//! queued through the bundle-pack path, **no hub and no RPC server**,
//! and asserts the component's trampoline comes up. The component list
//! rides an encode→decode round trip of the pack format first, so this
//! also covers the embed path the `aether-bundle-headless` bin runs:
//! pack → `AutoloadComponent` → `aether.component.load` mail → live
//! trampoline.
//!
//! Skipped when the probe wasm isn't pre-built (no wgpu gate — the
//! headless chassis needs no adapter); `AETHER_REQUIRE_RUNTIME=1`
//! flips the skip into a panic so CI catches a missing pre-build.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Test reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]

use std::env;
use std::fs;
use std::thread;
use std::time::Duration;

use aether_substrate_bundle::Chassis as _;
use aether_substrate_bundle::PersistOverride;
use aether_substrate_bundle::autoload::boot_manifest_autoload;
use aether_substrate_bundle::bundle_pack::{
    ChassisSettings, Pack, PackedComponent, decode_pack, encode_pack,
};
use aether_substrate_bundle::capabilities::http::HttpConfig;
use aether_substrate_bundle::capabilities::{AnthropicConfig, GeminiConfig, WasmTrampoline};
use aether_substrate_bundle::headless::{AutoloadComponent, HeadlessChassis, HeadlessEnv};
use aether_substrate_bundle::test_bench::test_helpers::{
    init_save_sandbox, locate_component_wasm, test_namespace_roots,
};

// Nested under `mod tests` so the nextest test name is
// `tests::heavy::…` and the `test(/::heavy::/)` serial-heavy filter
// matches it — a top-level `mod heavy` yields `heavy::…` (no leading
// `::`) and silently leaks into the parallel pool (#1564).
mod tests {
    mod heavy {
        use super::super::*;
        use std::time::Instant;

        #[test]
        fn autoloaded_component_comes_up_with_no_hub() {
            let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
            let Some(wasm_path) = locate_component_wasm("probe") else {
                assert!(
                    !strict,
                    "AETHER_REQUIRE_RUNTIME set but probe.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing it",
                );
                eprintln!(
                    "skipping: probe.wasm not built; \
                 run `cargo build --target wasm32-unknown-unknown -p aether-test-fixtures --examples`",
                );
                return;
            };
            let wasm = fs::read(&wasm_path).expect("read probe wasm");

            // Round-trip the component through the pack format — the same
            // bytes the bundle bin would embed and decode at boot.
            let pack = Pack {
                chassis: ChassisSettings::default(),
                components: vec![PackedComponent {
                    wasm,
                    config: Vec::new(),
                    name: Some("probe".to_owned()),
                    export: None,
                }],
            };
            let decoded = decode_pack(&encode_pack(&pack)).expect("pack round trip");

            // A hub-less headless env: no `rpc_addr`, no hub connection, and
            // persistence off so the boot touches no shared on-disk state.
            let env = HeadlessEnv {
                namespace_roots: test_namespace_roots(init_save_sandbox("headless-autoload")),
                http: HttpConfig::default(),
                http_server: None,
                anthropic: AnthropicConfig::default(),
                gemini: GeminiConfig::default(),
                tick_period: Duration::from_millis(16),
                rpc_addr: None,
                workers: None,
                ring_caps: aether_substrate_bundle::RingCapacities::default(),
                persist: PersistOverride::Argv(None),
                handle_store_max_bytes: None,
                autoload: decoded
                    .components
                    .into_iter()
                    .map(AutoloadComponent::from)
                    .collect(),
            };

            // `build` queues the autoload mail; the worker pool (up after
            // build) dispatches the load without the driver loop running,
            // so the trampoline appears without ever calling `run()`.
            let built = HeadlessChassis::build(env).expect("build headless chassis");
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if built.resolve_actor::<WasmTrampoline>("probe").is_some() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "autoloaded probe trampoline did not come up within 30s; live instances: {:?}",
                    built.resolve_actors::<WasmTrampoline>(),
                );
                thread::sleep(Duration::from_millis(25));
            }
            // Dropping `built` shuts the passives down in reverse boot order.
        }

        #[test]
        fn autoloaded_component_from_runtime_manifest_comes_up() {
            // The runtime-manifest twin of the embed test above: a real
            // `BundleManifest` JSON of *paths* is read by
            // `boot_manifest_autoload` — the same reader the chassis runs
            // for `AETHER_BOOT_MANIFEST`, the path a `spawn_substrate`
            // carrying a component list drives — and the resolved autoload
            // brings the probe up with no hub.
            let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
            let Some(wasm_path) = locate_component_wasm("probe") else {
                assert!(
                    !strict,
                    "AETHER_REQUIRE_RUNTIME set but probe.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing it",
                );
                eprintln!(
                    "skipping: probe.wasm not built; \
                 run `cargo build --target wasm32-unknown-unknown -p aether-test-fixtures --examples`",
                );
                return;
            };

            // Write a boot manifest of paths next to the test sandbox; the
            // reader resolves the wasm bytes itself.
            let sandbox = init_save_sandbox("headless-runtime-manifest");
            let manifest_path = sandbox.join("boot-manifest.json");
            let manifest_json = serde_json::json!({
                "components": [{ "wasm": wasm_path, "name": "probe" }],
            });
            fs::write(
                &manifest_path,
                serde_json::to_vec(&manifest_json).expect("serialize boot manifest"),
            )
            .expect("write boot manifest");

            let autoload = boot_manifest_autoload(&manifest_path).expect("read boot manifest");
            assert_eq!(autoload.len(), 1, "one component listed in the manifest");

            let env = HeadlessEnv {
                namespace_roots: test_namespace_roots(sandbox),
                http: HttpConfig::default(),
                http_server: None,
                anthropic: AnthropicConfig::default(),
                gemini: GeminiConfig::default(),
                tick_period: Duration::from_millis(16),
                rpc_addr: None,
                workers: None,
                ring_caps: aether_substrate_bundle::RingCapacities::default(),
                persist: PersistOverride::Argv(None),
                handle_store_max_bytes: None,
                autoload,
            };

            let built = HeadlessChassis::build(env).expect("build headless chassis");
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if built.resolve_actor::<WasmTrampoline>("probe").is_some() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "runtime-manifest probe trampoline did not come up within 30s; live instances: {:?}",
                    built.resolve_actors::<WasmTrampoline>(),
                );
                thread::sleep(Duration::from_millis(25));
            }
        }
    }
}
