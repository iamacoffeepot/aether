//! Headless boot-time autoload smoke (iamacoffeepot/aether#1529).
//!
//! Boots a real `HeadlessChassis` (not the substrate harness — the point is
//! the headless `Chassis::build` autoload drain) with a probe component
//! queued through the bundle-pack path, **no hub and no RPC server**,
//! and asserts the component's trampoline comes up. The component list
//! rides an encode→decode round trip of the pack format first, so this
//! also covers the embed path the `aether-bundle-headless` bin runs:
//! embedded `pack/manifest` → object resolution → `AutoloadComponent` →
//! `aether.component.load` mail → live trampoline.
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
use aether_chassis::bundle_pack::ChassisSettings;
use aether_chassis::package::{
    EmbeddedObjectStore, PackageEntry, PackageManifest, Sha256, embedded_autoload, encode_manifest,
};
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

        // Resolve the component through the embedded-package path — the same
        // decode + object-resolution the `aether-bundle-headless` bin runs on
        // its `include_bytes!`'d artifact. The object is keyed by a manifest
        // hash into an in-memory object store (the chassis never re-hashes
        // bytes — integrity is the store's job — so any consistent key works).
        let object = Sha256([0x11; 32]);
        let manifest = PackageManifest {
            settings: ChassisSettings::default(),
            entries: vec![PackageEntry {
                object,
                config: None,
                name: Some("probe".to_owned()),
                export: None,
                replicas: None,
            }],
        };
        let manifest_bytes = encode_manifest(&manifest);
        let object_hex = object.to_hex();
        let objects: &[(&str, &[u8])] = &[(object_hex.as_str(), wasm.as_slice())];
        let (_settings, autoload) =
            embedded_autoload(&manifest_bytes, &EmbeddedObjectStore::new(objects)).expect("embedded autoload");

        // A hub-less headless env: no `rpc_address`, no hub connection, and
        // persistence off so the boot touches no shared on-disk state. The tick
        // cadence resolves in `build` off `base.sources` (default 60 Hz here); this
        // test drives autoload off the worker pool, not ticks.
        let env = CommonEnv {
            base: ChassisBase {
                sources: default_sources(),
                actor_ring: ActorRingConfig::default(),
                scheduler_tuning: SchedulerTuningConfig::default(),
                settlement: SettlementConfig::default(),
            },
            namespace_roots: test_namespace_roots(init_save_sandbox("headless-autoload")),
            runtime: RuntimeConfig::default(),
            chassis_boot: ChassisBootConfig::default(),
            autoload,
            package_settings: ChassisSettings::default(),
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
