//! `FleetBench` `spawn_substrate` proof (issue 1451, Tier-A): fork+exec a
//! real `aether-substrate-headless` through the hub's engines cap, then
//! confirm the hub registered it in the supervised fleet.
//!
//! Heavy by construction — it forks a real process and settles across
//! it — so the test lives in `mod tests::heavy`. nextest's
//! `test(/::heavy::/)` selector keys on the `::heavy::` path segment to
//! put it in the `serial-heavy` group (serialized; soaked by
//! `scripts/flake-soak.sh`), so the marker module needs a parent here.

mod fleetbench;

mod tests {
    mod heavy {
        use crate::fleetbench::FleetBench;

        /// Spawn a headless substrate and assert it shows up in
        /// `ListEngines` with a fresh heartbeat — the real-process
        /// analog of the `spawn_substrate` → `list_engines` agent
        /// workflow.
        #[test]
        fn fleetbench_spawns_and_lists_a_real_headless_substrate() {
            let mut bench = FleetBench::start();
            let engine = bench.spawn_headless();
            let engine_id = engine.0.to_string();

            let engines = bench.list_engines();
            let descriptor = engines
                .iter()
                .find(|e| e.engine_id == engine_id)
                .unwrap_or_else(|| {
                    panic!("spawned engine {engine_id} should appear in ListEngines: {engines:?}")
                });

            // Freshly spawned ⇒ recently seen. The cap evicts a stale
            // engine at the miss limit (default 5s × 3); a just-registered
            // engine is well under that — ~0 at spawn per the
            // EngineDescriptor contract.
            assert!(
                descriptor.last_heartbeat_age_millis < 10_000,
                "freshly spawned engine should have a near-zero heartbeat age, got {}ms",
                descriptor.last_heartbeat_age_millis,
            );
            assert_ne!(
                descriptor.rpc_port, 0,
                "the cap reports the assigned RPC port"
            );
        }
    }
}
