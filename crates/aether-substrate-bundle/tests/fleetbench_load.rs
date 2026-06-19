//! `FleetBench` `load_component` proof (issue 1451, Tier-A): load a real
//! wasm component (the `probe` fixture, located through
//! `dist/manifest.json`) into a forked substrate and assert it registers
//! at its ADR-0099 lineage address.
//!
//! Heavy by construction (fork+exec + cross-process settle) — the test
//! lives in `mod tests::heavy` so nextest's `test(/::heavy::/)` selector
//! serializes it in the `serial-heavy` group (and soaks it).

mod fleetbench;

mod tests {
    mod heavy {
        use aether_actor::Addressable;
        use aether_capabilities::WasmTrampoline;
        use aether_data::Kind;
        use aether_kinds::{LoadComponent, LoadResult};

        use crate::fleetbench::{FleetBench, dist_manifest_present};

        /// Load the `probe` component and assert `LoadResult.name` is the
        /// `/`-rendered lineage
        /// `aether.component/aether.embedded:<NAMESPACE>` (ADR-0099
        /// §3/§4). The probe example's `Addressable::NAMESPACE` is
        /// `test_fixture_probe` — distinct from the wasm stem (`probe`),
        /// so this also pins that the registered name comes from the
        /// component's declared namespace, not the file name. Also
        /// asserts the recorded `CallRecord` trace captured the load
        /// round-trip, exercising the benchmark-ready trace object.
        #[test]
        fn fleetbench_loads_probe_at_its_lineage_address() {
            if !dist_manifest_present() {
                return;
            }
            let mut bench = FleetBench::start();
            let engine = bench.spawn_headless();
            let addr = bench.load(engine, "aether_test_fixtures_bundle");

            let expected = format!(
                "aether.component/{}:test_fixture_probe",
                WasmTrampoline::NAMESPACE,
            );
            assert_eq!(
                addr, expected,
                "LoadResult.name should be the ADR-0099 lineage address",
            );

            // The recorded trace captures the load as a first-class
            // CallRecord: a LoadComponent call to a forked engine that
            // drew a single LoadResult reply.
            let load_record = bench
                .calls()
                .iter()
                .find(|record| record.request_kind == <LoadComponent as Kind>::ID)
                .expect("the load round-trip is recorded as a CallRecord");
            assert_eq!(
                load_record.engine,
                Some(engine),
                "the load call is routed to the forked engine",
            );
            assert_eq!(
                load_record.reply_kinds,
                vec![<LoadResult as Kind>::ID],
                "the load call drew exactly one LoadResult reply",
            );
        }
    }
}
