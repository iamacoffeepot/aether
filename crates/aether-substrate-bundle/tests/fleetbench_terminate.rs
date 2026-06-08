//! `FleetBench` `terminate_substrate` + standalone `list_engines` proofs
//! (issue 1459, Tier-A): fork real `aether-substrate-headless` processes
//! through the hub's engines cap, then assert the supervised fleet tracks
//! the spawned set and that a `terminate` evicts an engine synchronously.
//!
//! Heavy by construction (fork+exec + cross-process settle) — the tests
//! live in `mod tests::heavy` so nextest's `test(/::heavy::/)` selector
//! serializes them in the `serial-heavy` group.

mod fleetbench;

mod tests {
    mod heavy {
        use crate::fleetbench::FleetBench;

        /// Spawn two headless substrates and assert both appear in
        /// `ListEngines` with fresh heartbeats — the standalone
        /// `list_engines` row: the hub's fleet table round-trips the
        /// spawned set, not just a single engine.
        #[test]
        fn fleetbench_lists_the_spawned_engine_set() {
            let mut bench = FleetBench::start();
            let first = bench.spawn_headless();
            let second = bench.spawn_headless();

            let engines = bench.list_engines();
            for engine in [first, second] {
                let engine_id = engine.0.to_string();
                let descriptor = engines
                    .iter()
                    .find(|e| e.engine_id == engine_id)
                    .unwrap_or_else(|| {
                        panic!(
                            "spawned engine {engine_id} should appear in ListEngines: {engines:?}"
                        )
                    });
                // Freshly spawned ⇒ recently seen; the cap evicts only at
                // the miss limit (default 5s × 3), far above this bound.
                assert!(
                    descriptor.last_heartbeat_age_millis < 10_000,
                    "freshly spawned engine should have a near-zero heartbeat age, got {}ms",
                    descriptor.last_heartbeat_age_millis,
                );
            }
        }

        /// Spawn one headless substrate, confirm it is supervised, then
        /// `terminate` it and assert it is gone from a follow-up
        /// `list_engines` — the `terminate_substrate` row. The engines
        /// cap removes the fleet entry synchronously before replying, so
        /// the eviction is visible immediately, with no heartbeat-miss
        /// wait.
        #[test]
        fn fleetbench_terminate_evicts_from_the_fleet() {
            let mut bench = FleetBench::start();
            let engine = bench.spawn_headless();
            let engine_id = engine.0.to_string();

            let before = bench.list_engines();
            assert!(
                before.iter().any(|e| e.engine_id == engine_id),
                "spawned engine {engine_id} should be supervised before terminate: {before:?}",
            );

            bench.terminate(engine);

            let after = bench.list_engines();
            assert!(
                after.iter().all(|e| e.engine_id != engine_id),
                "terminated engine {engine_id} should be gone from the fleet: {after:?}",
            );
        }
    }
}
