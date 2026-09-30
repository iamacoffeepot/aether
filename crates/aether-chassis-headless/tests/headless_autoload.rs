//! Headless boot-time autoload smoke (iamacoffeepot/aether#1529).
//!
//! Boots a real `HeadlessChassis` (not the substrate harness — the point is
//! the headless `Chassis::build` boot load) with a probe component queued
//! through the JSON boot-manifest path, **no hub and no RPC server**, and
//! asserts the component's trampoline is live when `build` returns: a
//! `BootManifest` of file paths → `boot_manifest_autoload` →
//! `AutoloadComponent` → one `Publish` of its module and one `Spawn` per
//! instance key, awaited to `Ok` → live trampoline (issue #6413, issue
//! #7155). This is the reader a `spawn_substrate` carrying a component list
//! drives through `AETHER_BOOT_MANIFEST`. A boot component that fails to
//! load fails the build; a `replicas: N` entry spawns N counter-keyed
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
        // A `replicas: 2` manifest entry must publish its module once and
        // spawn two counter-keyed instances of the selected (instanced)
        // export before `build` returns — the bug this catches is a
        // fan-out that spawns only one instance, or spawns both under the
        // same key (which the component host refuses as already live).
        // `test.ui.panel` (`aether_test_fixtures_bundle::Panel`) is
        // `#[actor(instanced, root)]` with no dependencies, so nothing else
        // in a fresh chassis draws a counter-keyed spawn ahead of it: its
        // two instances land at the spawner's first two counter keys, `0`
        // and `1`.
        //
        // There is no embedder hook to compose a custom reply-capturing
        // actor into a production `HeadlessChassis::build`, so unlike the
        // MCP `list_components`-based approach the issue's plan preferred,
        // this asserts liveness the same way the sibling test above does:
        // `built.resolve_address` on each instance's ADR-0241 §5 name.
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

        let sandbox = init_save_sandbox("headless-runtime-manifest-replicas");
        let manifest_path = sandbox.join("boot-manifest.json");
        let manifest_json = serde_json::json!({
            "components": [{ "wasm": wasm_path, "export": "test.ui.panel", "replicas": 2 }],
        });
        fs::write(&manifest_path, serde_json::to_vec(&manifest_json).expect("serialize boot manifest"))
            .expect("write boot manifest");

        let autoload = boot_manifest_autoload(&manifest_path).expect("read boot manifest");
        assert_eq!(autoload.len(), 1, "one AutoloadComponent carries both replicas' keys");
        assert_eq!(autoload[0].keys, vec![None, None], "replicas: 2 is two counter-keyed instance keys");

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
            // `export` is set so `expand_replicas` never inspects the wasm
            // for a declared default namespace: the point of this test is
            // that a genuinely malformed module fails at `Publish` (build
            // time), not at the manifest read that precedes it.
            "components": [{ "wasm": wasm_path, "name": "broken", "export": "irrelevant" }],
        });
        fs::write(&manifest_path, serde_json::to_vec(&manifest_json).expect("serialize boot manifest"))
            .expect("write boot manifest");

        let autoload = boot_manifest_autoload(&manifest_path).expect("read boot manifest");
        let error = HeadlessChassis::build(headless_env(sandbox, autoload))
            .expect_err("a boot component that fails to load must fail the build");
        assert!(error.to_string().contains("broken"), "the build error must name the failing entry: {error}");
    }
}
