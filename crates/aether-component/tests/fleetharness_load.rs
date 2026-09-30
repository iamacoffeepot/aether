//! `FleetHarness` `load_component` proof (issue 1451, Tier-A): load a real
//! wasm component (the bundle's `QuietProbe` export, located through
//! `dist/manifest.json`) into a forked substrate and assert it registers
//! at its ADR-0099 lineage address.

mod tests {
    use aether_data::Kind;
    use aether_kinds::{LoadComponent, LoadResult};

    use aether_harness_fleet::{FleetHarness, dist_component_available, read_component_wasm};

    /// Load the bundle's `QuietProbe` export and assert `LoadResult.path` is
    /// the guest's own published name, `<NAMESPACE>` for a singleton
    /// (ADR-0241 §5). The selected export's `Addressable::NAMESPACE` is
    /// `test.quiet_probe` — distinct from the wasm stem
    /// (`aether_test_fixtures_bundle`), so this also pins that the
    /// registered name comes from the selected export's namespace, not the
    /// file name. Also asserts the recorded `CallRecord` trace captured the
    /// load round-trip, exercising the benchmark-ready trace object.
    #[test]
    fn fleetharness_loads_probe_at_its_lineage_address() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let addr = harness
            .load(
                engine,
                &LoadComponent {
                    wasm: read_component_wasm("aether_test_fixtures_bundle"),
                    name: None,
                    config: Vec::new(),
                    export: Some("test.quiet_probe".to_owned()),
                },
            )
            .addr;

        assert_eq!(addr, "test.quiet_probe", "LoadResult.path should be the guest's published name");

        // The recorded trace captures the load as a first-class
        // CallRecord: a LoadComponent call to a forked engine that
        // drew a single LoadResult reply.
        let load_record = harness
            .calls()
            .iter()
            .find(|record| record.request_kind == <LoadComponent as Kind>::ID)
            .expect("the load round-trip is recorded as a CallRecord");
        assert_eq!(load_record.engine, Some(engine), "the load call is routed to the forked engine");
        assert_eq!(
            load_record.reply_kinds,
            vec![<LoadResult as Kind>::ID],
            "the load call drew exactly one LoadResult reply",
        );
    }
}
