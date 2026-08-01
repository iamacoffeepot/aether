//! `FleetHarness` `terminate_substrate` + standalone `list_engines` proofs
//! (issue 1459, Tier-A): fork real `aether-headless` processes
//! through the hub's engines cap, then assert the supervised fleet tracks
//! the spawned set and that a `terminate` evicts an engine synchronously.

mod tests {
    use aether_harness_fleet::{FleetHarness, poll_until};
    use aether_kinds::DeathReason;

    /// Spawn two headless substrates and assert both round-trip through
    /// `ListEngines` — the standalone `list_engines` row: the hub's fleet
    /// table reflects the spawned set, not just a single engine.
    #[test]
    fn fleetharness_lists_the_spawned_engine_set() {
        let mut harness = FleetHarness::start();
        let first = harness.spawn_headless();
        let second = harness.spawn_headless();
        let wanted = [first.0.to_string(), second.0.to_string()];

        // Assert both spawned engines appear in the fleet table. Registration
        // is synchronous with the spawn reply (the hub holds
        // `SpawnEngineResult::Ok` until its proxy connects), so the poll just
        // absorbs any async slack under load rather than depending on a wall
        // clock. Heartbeat freshness is deliberately not asserted here:
        // FleetHarness runs with the heartbeat disabled (`FleetConfig`
        // default), so `last_heartbeat_age_millis` only measures time since
        // registration, not liveness — the pong-refresh / miss-eviction logic
        // is covered deterministically by the `engine::proxy` unit tests.
        let listed = poll_until(|| {
            let engines = harness.list_engines();
            wanted.iter().all(|id| engines.iter().any(|e| &e.engine_id == id))
        });
        assert!(listed, "both spawned engines should round-trip through ListEngines: {:?}", harness.list_engines());
    }

    /// Spawn one headless substrate, confirm it is supervised, then
    /// `terminate` it and assert it is gone from a follow-up
    /// `list_engines` — the `terminate_substrate` row. The engines
    /// cap removes the fleet entry synchronously before replying, so
    /// the eviction is visible immediately, with no heartbeat-miss
    /// wait.
    #[test]
    fn fleetharness_terminate_evicts_from_the_fleet() {
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let engine_id = engine.0.to_string();

        let before = harness.list_engines();
        assert!(
            before.iter().any(|e| e.engine_id == engine_id),
            "spawned engine {engine_id} should be supervised before terminate: {before:?}",
        );

        harness.terminate(engine);

        let after = harness.list_engines();
        assert!(
            after.iter().all(|e| e.engine_id != engine_id),
            "terminated engine {engine_id} should be gone from the fleet: {after:?}",
        );
    }

    /// Spawn one headless substrate, `terminate` it, then assert it
    /// surfaces in the `recently_died` ring with reason `Terminated` —
    /// the issue-1906 row: a removed engine carries *why* it left, so
    /// a deliberate shutdown is distinguishable from a crash. Drives
    /// the deliberate-terminate recording path (`on_terminate` records
    /// `Terminated` at the removal site) end-to-end against a real
    /// engine, which the engines-cap unit tests can't seed without
    /// forking a substrate.
    #[test]
    fn fleetharness_terminate_records_death_reason() {
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let engine_id = engine.0.to_string();

        harness.terminate(engine);

        let dead = harness.recently_died();
        let record = dead
            .iter()
            .find(|d| d.engine_id == engine_id)
            .unwrap_or_else(|| panic!("terminated engine {engine_id} should appear in recently_died: {dead:?}"));
        assert_eq!(
            record.reason,
            DeathReason::Terminated,
            "a deliberate terminate is recorded as Terminated, got {:?}",
            record.reason,
        );
    }
}
