//! Headless boot-time autoload smoke (iamacoffeepot/aether#1529).
//!
//! Boots a real `HeadlessChassis` (not the substrate harness — the point is
//! the headless `Chassis::build` autoload drain) with a probe component
//! queued through the JSON boot-manifest path, **no hub and no RPC server**,
//! and asserts the component's trampoline comes up: a `BootManifest` of file
//! paths → `boot_manifest_autoload` → `AutoloadComponent` →
//! `aether.component.load` mail → live trampoline. This is the reader a
//! `spawn_substrate` carrying a component list drives through
//! `AETHER_BOOT_MANIFEST`.
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

use aether_chassis::autoload::boot_manifest_autoload;
use aether_chassis::boot::{
    ActorRingConfig, ChassisBase, ChassisBootConfig, CommonEnv, RuntimeConfig, SchedulerTuningConfig, SettlementConfig,
};
use aether_chassis::boot_manifest::ChassisSettings;
use aether_chassis_headless::HeadlessChassis;
use aether_component::WasmTrampoline;
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
    /// in-code equivalent of the argv/env/file layers `CommonEnv::resolve`
    /// assembles. The builder resolves each composed cap's `Config` off this.
    fn default_sources() -> ConfigSources {
        let mut sources = ConfigSources::new(None);
        sources.set_override(HttpConfig::default());
        sources.set_override(HttpServerConfig::default());
        sources.set_override(LifecycleConfig { advance_timeout_millis: 1_000 });
        sources
    }

    #[test]
    fn autoloaded_component_from_runtime_manifest_comes_up() {
        // A real `BootManifest` JSON of *paths* is read by
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

        let env = CommonEnv {
            base: ChassisBase {
                sources: default_sources(),
                actor_ring: ActorRingConfig::default(),
                scheduler_tuning: SchedulerTuningConfig::default(),
                settlement: SettlementConfig::default(),
            },
            namespace_roots: test_namespace_roots(sandbox),
            runtime: RuntimeConfig::default(),
            chassis_boot: ChassisBootConfig::default(),
            autoload,
            package_settings: ChassisSettings::default(),
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
