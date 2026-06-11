//! `FleetBench` `replace_component` proof (issue 1459, Tier-A): load the
//! `probe` fixture into a forked substrate, then atomically swap it for
//! the `aether_camera` wasm at the same trampoline mailbox id (ADR-0022)
//! and assert the returned capability set reflects the new binary while
//! the lineage address stays put.
//!
//! Heavy by construction (fork+exec + cross-process settle) — the test
//! lives in `mod tests::heavy` so nextest's `test(/::heavy::/)` selector
//! serializes it in the `serial-heavy` group.

mod fleetbench;

mod tests {
    mod heavy {
        use aether_actor::Actor;
        use aether_camera::CameraCreate;
        use aether_capabilities::WasmTrampoline;
        use aether_data::Kind;
        use aether_kinds::{ComponentCapabilities, LogTailResult, Tick};
        use aether_test_fixtures::SetRender;

        use crate::fleetbench::{FleetBench, dist_manifest_present};

        /// Load `probe` (handlers `SetRender` + `Tick`), then `replace`
        /// it with `aether_camera` (handlers `CameraCreate` + `Tick` +
        /// the camera-driver kinds) targeting the captured trampoline
        /// `mailbox_id`. The returned `ReplaceResult::Ok.capabilities`
        /// must carry the camera handler set and not the probe's, with
        /// `Tick` surviving the swap; the lineage address — unchanged by
        /// construction, since the trampoline keeps its load-time name —
        /// must still route to the live mailbox afterward.
        #[test]
        fn fleetbench_replaces_probe_with_camera_at_a_stable_address() {
            if !dist_manifest_present() {
                return;
            }
            let mut bench = FleetBench::start();
            let engine = bench.spawn_headless();
            let loaded = bench.load_full(engine, "probe");

            let has = |caps: &ComponentCapabilities, id| caps.handlers.iter().any(|h| h.id == id);

            // Pre-replace sanity: the probe declares SetRender, not the
            // camera's create kind, and registers at its ADR-0099 lineage
            // address.
            assert!(
                has(&loaded.capabilities, SetRender::ID),
                "probe should declare a SetRender handler: {:?}",
                loaded.capabilities.handlers,
            );
            assert!(
                !has(&loaded.capabilities, CameraCreate::ID),
                "probe should not declare a CameraCreate handler: {:?}",
                loaded.capabilities.handlers,
            );
            let expected = format!(
                "aether.component/{}:test_fixture_probe",
                WasmTrampoline::NAMESPACE,
            );
            assert_eq!(
                loaded.addr, expected,
                "probe should load at its ADR-0099 lineage address",
            );

            let caps = bench.replace(engine, loaded.mailbox_id, "aether_camera");

            // Post-replace: the camera handler set is active, the probe's
            // is gone, and Tick (declared by both) survives the swap.
            assert!(
                has(&caps, CameraCreate::ID),
                "post-replace should declare a CameraCreate handler: {:?}",
                caps.handlers,
            );
            assert!(
                !has(&caps, SetRender::ID),
                "post-replace should not declare the probe's SetRender handler: {:?}",
                caps.handlers,
            );
            assert!(
                has(&caps, Tick::ID),
                "Tick is declared by both components and should survive the swap: {:?}",
                caps.handlers,
            );

            // The lineage address still resolves to the live mailbox: a
            // LogTail routed to the rendered path is answered Ok, proving
            // the same mailbox was swapped in place.
            assert!(
                matches!(
                    bench.log_tail(engine, &loaded.addr, None),
                    LogTailResult::Ok { .. },
                ),
                "the lineage address should still route to the live mailbox after replace",
            );
        }
    }
}
