//! Headless boot-time autoload smoke (iamacoffeepot/aether#1529).
//!
//! Boots a real `HeadlessChassis` (not the substrate harness — the point is
//! the headless `Chassis::build` boot load) with a probe component queued
//! through the JSON boot-manifest path, **no hub and no RPC server**, and
//! asserts the component's trampoline is live when `build` returns: a
//! `BootManifest` of file paths → `boot_manifest_autoload` →
//! `AutoloadComponent` → one `Publish` of its module and one `Spawn` per
//! instance key, each bringing the module's code, awaited to `Ok` → live
//! trampoline (issue #6413, issue #7155). This is the reader a `spawn_substrate` carrying a component list
//! drives through `AETHER_BOOT_MANIFEST`. A boot component that fails to
//! load fails the build, as does one whose `wire` returns an error, and so
//! does an entry naming no export of a module
//! that exports several; a `replicas: N` entry spawns N counter-keyed
//! instances behind the one publish.
//!
//! The probe test is skipped when the probe wasm isn't pre-built (no wgpu
//! gate — the headless chassis needs no adapter); `AETHER_REQUIRE_RUNTIME=1`
//! flips the skip into a panic so CI catches a missing pre-build.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Test reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]

use std::env;
use std::fs;
use std::path::Path;

use aether_chassis::autoload::{AutoloadComponent, boot_manifest_autoload};
use aether_chassis::boot::{
    ActorRingConfig, ChassisBase, ChassisBootConfig, CommonEnv, RegistryQueueConfig, RuntimeConfig,
    SchedulerTuningConfig, SettlementConfig,
};
use aether_chassis::boot_manifest::ChassisSettings;
use aether_chassis_headless::HeadlessChassis;
use aether_data::ErasedActorPath;
use aether_harness_substrate_capture::test_helpers::{init_save_sandbox, locate_component_wasm, test_namespace_roots};
use aether_http::{HttpConfig, HttpServerConfig};
use aether_lifecycle::LifecycleConfig;
use aether_substrate::Chassis as _;
use aether_substrate::config::ConfigSources;

mod tests {
    use super::*;

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

    /// The hub-less headless env over `sandbox` that boots `autoload`.
    fn headless_env(sandbox: &Path, autoload: Vec<AutoloadComponent>) -> CommonEnv {
        CommonEnv {
            base: ChassisBase {
                sources: default_sources(),
                actor_ring: ActorRingConfig::default(),
                scheduler_tuning: SchedulerTuningConfig::default(),
                registry_queues: RegistryQueueConfig::default(),
                settlement: SettlementConfig::default(),
            },
            namespace_roots: test_namespace_roots(sandbox),
            runtime: RuntimeConfig::default(),
            chassis_boot: ChassisBootConfig::default(),
            autoload,
            package_settings: ChassisSettings::default(),
        }
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
            "components": [{ "wasm": wasm_path, "export": "test.quiet_probe" }],
        });
        fs::write(&manifest_path, serde_json::to_vec(&manifest_json).expect("serialize boot manifest"))
            .expect("write boot manifest");

        let autoload = boot_manifest_autoload(&manifest_path).expect("read boot manifest");
        assert_eq!(autoload.len(), 1, "one component listed in the manifest");

        // `build` returns only once every boot component has answered its
        // load, so the probe resolves at once, with no wait.
        let built = HeadlessChassis::build(headless_env(sandbox, autoload)).expect("build headless chassis");
        let address = ErasedActorPath::new("test.quiet_probe").expect("a well-formed actor path");
        let resolved = built.resolve_address(&address);
        assert!(resolved.is_ok(), "boot component {address} is not live when build returns: {resolved:?}");
    }

    #[test]
    fn replicated_boot_component_spawns_every_instance() {
        // A `replicas: 2` entry must stand up two counter-keyed instances of
        // its instanced export before `build` returns; the bug this catches
        // is a fan-out that spawns one instance, or both under one key.
        // `test.ui.panel` is the first counter-keyed spawn in a fresh
        // chassis, so its instances are `:0` and `:1`, resolved as the
        // sibling test resolves its entry.
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

        // The sandbox is shared per process, so this test's manifest carries
        // its own name.
        let sandbox = init_save_sandbox("headless-runtime-manifest");
        let manifest_path = sandbox.join("replicas-boot-manifest.json");
        let manifest_json = serde_json::json!({
            "components": [{ "wasm": wasm_path, "export": "test.ui.panel", "replicas": 2 }],
        });
        fs::write(&manifest_path, serde_json::to_vec(&manifest_json).expect("serialize boot manifest"))
            .expect("write boot manifest");

        let autoload = boot_manifest_autoload(&manifest_path).expect("read boot manifest");

        // `build` returns only once every boot component's publish and
        // every one of its spawns has answered, so both instances resolve
        // at once, with no wait.
        let built = HeadlessChassis::build(headless_env(sandbox, autoload)).expect("build headless chassis");
        for key in ["0", "1"] {
            let address = ErasedActorPath::new(&format!("test.ui.panel:{key}")).expect("a well-formed actor path");
            let resolved = built.resolve_address(&address);
            assert!(resolved.is_ok(), "replica instance {address} is not live when build returns: {resolved:?}");
        }
    }

    #[test]
    fn boot_entry_naming_no_export_of_a_multi_export_module_fails_the_build() {
        // An entry with no `export` of a module exporting several types must
        // fail the build naming its exports (ADR-0241 §9); the bug this
        // catches is autoload silently spawning one export of the module.
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

        // The sandbox is shared per process, so this test's manifest carries
        // its own name.
        let sandbox = init_save_sandbox("headless-runtime-manifest");
        let manifest_path = sandbox.join("unselected-boot-manifest.json");
        let manifest_json = serde_json::json!({ "components": [{ "wasm": wasm_path }] });
        fs::write(&manifest_path, serde_json::to_vec(&manifest_json).expect("serialize boot manifest"))
            .expect("write boot manifest");

        let autoload = boot_manifest_autoload(&manifest_path).expect("read boot manifest");
        let error = HeadlessChassis::build(headless_env(sandbox, autoload))
            .expect_err("an unselected entry of a multi-export module must fail the build")
            .to_string();
        for export in ["test.probe", "test.quiet_probe"] {
            assert!(error.contains(export), "the build error must name the export {export}: {error}");
        }
    }

    #[test]
    fn boot_component_whose_wire_fails_fails_the_build() {
        // A boot entry whose guest returns an error from `wire` must fail
        // the build with the guest's message (ADR-0247 rule 3); the bug this
        // catches is the loader counting it loaded, with the instance live
        // and unwired.
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

        // The sandbox is shared per process, so this test's manifest carries
        // its own name.
        let sandbox = init_save_sandbox("headless-runtime-manifest");
        let manifest_path = sandbox.join("wire-refuser-boot-manifest.json");
        let manifest_json = serde_json::json!({
            "components": [{ "wasm": wasm_path, "export": "test.wire_refuser" }],
        });
        fs::write(&manifest_path, serde_json::to_vec(&manifest_json).expect("serialize boot manifest"))
            .expect("write boot manifest");

        let autoload = boot_manifest_autoload(&manifest_path).expect("read boot manifest");
        let error = HeadlessChassis::build(headless_env(sandbox, autoload))
            .expect_err("a boot component whose wire fails must fail the build")
            .to_string();
        assert!(error.contains("wire refused"), "the build error must carry the guest's message: {error}");
    }

    #[test]
    fn boot_component_that_fails_to_load_fails_the_build() {
        // A boot entry whose bytes are not wasm must fail the build, naming
        // the entry, rather than leave a half-booted engine running.
        // The sandbox is shared per process, so this test's files carry their
        // own names.
        let sandbox = init_save_sandbox("headless-runtime-manifest");
        let wasm_path = sandbox.join("broken.wasm");
        fs::write(&wasm_path, b"not a wasm module").expect("write broken component bytes");
        let manifest_path = sandbox.join("broken-boot-manifest.json");
        let manifest_json = serde_json::json!({
            "components": [{ "wasm": wasm_path, "name": "broken" }],
        });
        fs::write(&manifest_path, serde_json::to_vec(&manifest_json).expect("serialize boot manifest"))
            .expect("write boot manifest");

        let autoload = boot_manifest_autoload(&manifest_path).expect("read boot manifest");
        let error = HeadlessChassis::build(headless_env(sandbox, autoload))
            .expect_err("a boot component that fails to load must fail the build");
        assert!(error.to_string().contains("broken"), "the build error must name the failing entry: {error}");
    }
}
