//! Headless boot-time autoload smoke (iamacoffeepot/aether#1529).
//!
//! Boots a real `HeadlessChassis` (not the substrate harness — the point is
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

use aether_anthropic::AnthropicConfig;
use aether_chassis::autoload::boot_manifest_autoload;
use aether_chassis::boot::RuntimeConfig;
use aether_chassis::bundle_pack::{ChassisSettings, Pack, PackedComponent, decode_pack, encode_pack};
use aether_chassis_headless::{AutoloadComponent, HeadlessChassis, HeadlessEnv};
use aether_component::WasmTrampoline;
use aether_contentgen::ContentGenConfig;
use aether_gemini::GeminiConfig;
use aether_harness_substrate_capture::test_helpers::{init_save_sandbox, locate_component_wasm, test_namespace_roots};
use aether_http::{HttpConfig, HttpServerConfig};
use aether_lifecycle::LifecycleConfig;
use aether_substrate::Chassis as _;
use aether_substrate::config::ConfigSources;

mod tests {
    use super::*;
    use std::time::Instant;

    /// ADR-0156 §5: the cap configs a hub-less headless autoload boot needs,
    /// staged as programmatic overrides on the builder's source stack — the
    /// in-code equivalent of the argv/env/file layers `HeadlessEnv::from_env`
    /// assembles. The builder resolves each composed cap's `Config` off this.
    fn default_sources() -> ConfigSources {
        let mut sources = ConfigSources::new(None);
        sources.set_override(HttpConfig::default());
        sources.set_override(HttpServerConfig::default());
        sources.set_override(AnthropicConfig::default());
        sources.set_override(GeminiConfig::default());
        sources.set_override(LifecycleConfig { advance_timeout_millis: 1_000 });
        sources
    }

    #[test]
    fn autoloaded_component_comes_up_with_no_hub() {
        let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
        let Some(wasm_path) = locate_component_wasm("aether_test_fixtures_bundle") else {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but probe.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing it",
            );
            eprintln!(
                "skipping: probe.wasm not built; \
                 run `cargo build --target wasm32-unknown-unknown -p aether-test-fixtures-bundle`",
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
                replicas: None,
            }],
        };
        let decoded = decode_pack(&encode_pack(&pack)).expect("pack round trip");

        // A hub-less headless env: no `rpc_addr`, no hub connection, and
        // persistence off so the boot touches no shared on-disk state.
        let env = HeadlessEnv {
            namespace_roots: test_namespace_roots(init_save_sandbox("headless-autoload")),
            sources: default_sources(),
            contentgen: ContentGenConfig::default(),
            tick_period: Duration::from_millis(16),
            runtime: RuntimeConfig::default(),
            workers: None,
            ring_caps: aether_substrate::RingCapacities::default(),
            scheduler_tuning: aether_substrate::SchedulerTuning::default(),
            teardown_cap: Duration::from_millis(100),
            autoload: decoded.components.into_iter().map(AutoloadComponent::from).collect(),
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
        let Some(wasm_path) = locate_component_wasm("aether_test_fixtures_bundle") else {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but probe.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing it",
            );
            eprintln!(
                "skipping: probe.wasm not built; \
                 run `cargo build --target wasm32-unknown-unknown -p aether-test-fixtures-bundle`",
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
        fs::write(&manifest_path, serde_json::to_vec(&manifest_json).expect("serialize boot manifest"))
            .expect("write boot manifest");

        let autoload = boot_manifest_autoload(&manifest_path).expect("read boot manifest");
        assert_eq!(autoload.len(), 1, "one component listed in the manifest");

        let env = HeadlessEnv {
            namespace_roots: test_namespace_roots(sandbox),
            sources: default_sources(),
            contentgen: ContentGenConfig::default(),
            tick_period: Duration::from_millis(16),
            runtime: RuntimeConfig::default(),
            workers: None,
            ring_caps: aether_substrate::RingCapacities::default(),
            scheduler_tuning: aether_substrate::SchedulerTuning::default(),
            teardown_cap: Duration::from_millis(100),
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
