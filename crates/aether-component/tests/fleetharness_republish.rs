//! `FleetHarness` publish + spawn proof (issue 1459, Tier-A; ADR-0241 §3,
//! §7, §9): publish a module over the real hub → RPC → forked-substrate
//! wire and assert a republish moves every live instance of its namespaces
//! in place, at the same lineage address and mailbox id (ADR-0022), with the
//! reply naming each published type; and a spawn of a published singleton
//! answers `Spawned`, while a second spawn of the same name answers `Live`
//! at the same path with nothing stood up twice.

mod tests {
    use aether_data::Kind;
    use aether_kinds::{ComponentCapabilities, LoadComponent, LogTailResult, PublishedType, Spawn, SpawnResult};
    use aether_substrate::testing::successor_wasm;
    use aether_test_fixtures_kinds::{GateProbe, GateQuery};

    use aether_harness_fleet::{FleetHarness, dist_component_available, read_component_wasm};

    fn has(capabilities: &ComponentCapabilities, id: aether_data::KindId) -> bool {
        capabilities.handlers.iter().any(|handler| handler.id == id)
    }

    fn published<'a>(types: &'a [PublishedType], namespace: &str) -> &'a ComponentCapabilities {
        &types
            .iter()
            .find(|published| published.namespace == namespace)
            .unwrap_or_else(|| panic!("the publish reports {namespace}: {types:?}"))
            .capabilities
    }

    /// Load the gate pair's first version, whose `test.republish.gate` has no
    /// `GateProbe` row, then republish the second over the wire. The reply
    /// must report the gate with its added `GateProbe` row and its kept
    /// `GateQuery` row, and the loaded address must still route to the live
    /// mailbox afterward.
    ///
    /// Catches: a republish that answers from the predecessor's surface, or
    /// one that re-spawns the instance under a new name instead of swapping
    /// it in place.
    #[test]
    fn fleetharness_republishes_a_module_at_a_stable_address() {
        if !dist_component_available("republish_group_v1") || !dist_component_available("republish_group_v2") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let loaded = harness.load(
            engine,
            &LoadComponent {
                wasm: read_component_wasm("republish_group_v1"),
                name: None,
                config: Vec::new(),
                export: Some("test.republish.gate".to_owned()),
            },
        );

        assert!(has(&loaded.capabilities, GateQuery::ID), "v1 declares GateQuery: {:?}", loaded.capabilities);
        assert!(!has(&loaded.capabilities, GateProbe::ID), "v1 declares no GateProbe: {:?}", loaded.capabilities);

        let types = harness.publish(engine, read_component_wasm("republish_group_v2"));

        let gate = published(&types, "test.republish.gate");
        assert!(has(gate, GateProbe::ID), "the republished gate declares its added GateProbe row: {gate:?}");
        assert!(has(gate, GateQuery::ID), "the kept GateQuery row survives the republish: {gate:?}");
        assert!(
            matches!(harness.log_tail(engine, &loaded.addr, None, None), LogTailResult::Ok { .. }),
            "the lineage address should still route to the live mailbox after the republish",
        );
    }

    /// A republish moves every live instance of every namespace its module
    /// publishes (ADR-0241 §7): two instances of two different module
    /// exports both still serve at their addresses after the module is
    /// republished with identical code under a new hash, and the reply
    /// names both types. The courier pair's first version depends on no
    /// capability, so both exports load on the headless engine.
    ///
    /// Catches: a republish that swaps only one instance, or reports only
    /// the types that had one.
    #[test]
    fn fleetharness_republish_moves_every_instance_of_the_module() {
        if !dist_component_available("republish_courier_v1") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let courier = harness.load(
            engine,
            &LoadComponent {
                wasm: read_component_wasm("republish_courier_v1"),
                name: None,
                config: Vec::new(),
                export: Some("test.republish.courier".to_owned()),
            },
        );
        let parcel = harness.load(
            engine,
            &LoadComponent {
                wasm: read_component_wasm("republish_courier_v1"),
                name: None,
                config: Vec::new(),
                export: Some("test.republish.parcel".to_owned()),
            },
        );

        let types = harness.publish(engine, successor_wasm(&read_component_wasm("republish_courier_v1"), 1));

        published(&types, "test.republish.courier");
        published(&types, "test.republish.parcel");
        for address in [&courier.addr, &parcel.addr] {
            assert!(
                matches!(harness.log_tail(engine, address, None, None), LogTailResult::Ok { .. }),
                "{address} should still route to its live mailbox after the republish",
            );
        }
    }

    /// Publish the fixture bundle, then spawn its `test.quiet_probe`
    /// singleton (ADR-0241 §9): the first spawn stands the instance up and
    /// answers `Spawned` at its published name; a second spawn of the same
    /// name answers `Live` at the same path, naming the instance that is
    /// already there rather than re-initialising it.
    ///
    /// Catches: a `Spawn` that does not decode or route over the wire, or
    /// one that re-initialises or refuses a live name instead of naming it.
    #[test]
    fn fleetharness_spawn_of_a_live_name_answers_live_at_the_same_path() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        harness.publish(engine, read_component_wasm("aether_test_fixtures_bundle"));

        // A spawn of a published type always builds an instance that can read
        // its assets, in every hook, from its own module (ADR-0250).
        let request = Spawn { namespace: "test.quiet_probe".to_owned(), key: None, parent: None, config: Vec::new() };

        let first_path = match harness.spawn(engine, &request) {
            SpawnResult::Spawned { path, .. } => path,
            other => panic!("the first spawn of an absent name should answer Spawned, got {other:?}"),
        };
        assert_eq!(first_path.to_string(), "test.quiet_probe", "the spawn registers at its published namespace");

        let second_path = match harness.spawn(engine, &request) {
            SpawnResult::Live { path, .. } => path,
            other => panic!("a spawn of a live name should answer Live, got {other:?}"),
        };
        assert_eq!(second_path, first_path, "the second spawn names the same instance, not a fresh one");
    }
}
