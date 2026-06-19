//! `FleetBench` `replace_component` proof (issue 1459, Tier-A): load the
//! `probe` fixture into a forked substrate, then atomically swap it for
//! `aether-kit`'s `aether.camera` export (selector `aether_kit@aether.camera`) at the
//! same trampoline mailbox id (ADR-0022) and assert the returned
//! capability set reflects the new binary while the lineage address
//! stays put.

mod fleetbench;

mod tests {
    use aether_actor::Addressable;
    use aether_capabilities::WasmTrampoline;
    use aether_data::Kind;
    use aether_kinds::{ComponentCapabilities, LogTailResult, Ping, Tick};
    use aether_kit::camera::CameraCreate;
    use aether_test_fixtures_kinds::SetRender;

    use crate::fleetbench::{FleetBench, dist_manifest_present};

    /// Load `probe` (handlers `SetRender` + `Tick`), then `replace`
    /// it with `aether-kit`'s non-entry `aether.camera` export (selector
    /// `aether_kit@aether.camera`; handlers `CameraCreate` + `Tick` + the
    /// camera-driver kinds) targeting the captured trampoline
    /// `mailbox_id` — exercising `ReplaceComponent.export` (#2027)
    /// end-to-end over the wire. The returned
    /// `ReplaceResult::Ok.capabilities` must carry the camera
    /// handler set and not the probe's, with `Tick` surviving the
    /// swap; the lineage address — unchanged by construction, since
    /// the trampoline keeps its load-time name — must still route to
    /// the live mailbox afterward.
    #[test]
    fn fleetbench_replaces_probe_with_camera_at_a_stable_address() {
        if !dist_manifest_present() {
            return;
        }
        let mut bench = FleetBench::start();
        let engine = bench.spawn_headless();
        let loaded = bench.load_full(engine, "aether_test_fixtures_bundle");

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

        let caps = bench.replace_export(engine, loaded.mailbox_id, "aether_kit", "aether.camera");

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

    /// ADR-0096 wire regression for `ReplaceComponent.export`: load
    /// the `multi_actor` module's **entry** actor (`RootManager`, a
    /// strict receiver — no `#[fallback]`), then replace it with the
    /// non-entry export `ui.panel` (`Panel`, which carries a
    /// `#[fallback]`) at the same trampoline `mailbox_id`. The
    /// post-replace capabilities must be `Panel`'s — `fallback`
    /// flips from `None` to `Some` — proving the new `export` field
    /// survived the real `Call` wire and drove the trampoline's
    /// effective-tag selection to a non-entry actor (which a bare
    /// replace, reusing the hosted entry tag, could never reach).
    /// `FleetBench` is the right harness: the field must round-trip the
    /// wire, not just the in-process path.
    #[test]
    fn fleetbench_replace_targets_a_non_entry_export() {
        if !dist_manifest_present() {
            return;
        }
        let mut bench = FleetBench::start();
        let engine = bench.spawn_headless();

        // Load the `RootManager` actor (a strict receiver) from the
        // bundle by its `ui.root` export. It is a non-entry actor in the
        // bundle (the entry is `Probe`), so it is selected explicitly.
        let loaded = bench.load_full_export(engine, "aether_test_fixtures_bundle", "ui.root");

        // Pre-replace: the entry is a strict receiver — it declares a
        // Ping handler and no fallback.
        assert!(
            loaded
                .capabilities
                .handlers
                .iter()
                .any(|h| h.id == Ping::ID),
            "the entry RootManager should declare a Ping handler: {:?}",
            loaded.capabilities.handlers,
        );
        assert!(
            loaded.capabilities.fallback.is_none(),
            "the entry RootManager is a strict receiver — no fallback: {:?}",
            loaded.capabilities.fallback,
        );

        // Replace into the non-entry export `ui.panel`, at the same
        // mailbox id, carrying the export over the wire.
        let caps = bench.replace_export(
            engine,
            loaded.mailbox_id,
            "aether_test_fixtures_bundle",
            "ui.panel",
        );

        // Post-replace: Panel's capability group is active — still a
        // Ping handler, but now with a fallback, the observable
        // distinction the fixture is built to expose.
        assert!(
            caps.handlers.iter().any(|h| h.id == Ping::ID),
            "Panel should declare a Ping handler: {:?}",
            caps.handlers,
        );
        assert!(
            caps.fallback.is_some(),
            "the non-entry Panel carries a #[fallback]; the export-targeted \
                 replace should surface it: {:?}",
            caps.fallback,
        );

        // The lineage address still routes to the live mailbox: the
        // same trampoline was swapped in place, now hosting Panel.
        assert!(
            matches!(
                bench.log_tail(engine, &loaded.addr, None),
                LogTailResult::Ok { .. },
            ),
            "the lineage address should still route to the live mailbox after an \
                 export-targeted replace",
        );
    }
}
