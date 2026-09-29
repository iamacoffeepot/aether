//! `FleetHarness` `replace_component` proof (issue 1459, Tier-A; ADR-0241
//! §7): republish a module over the real hub → RPC → forked-substrate wire
//! and assert every live instance of its namespaces moved in place, at the
//! same lineage address and mailbox id (ADR-0022), with the reply naming
//! each republished type.

mod tests {
    use aether_data::Kind;
    use aether_kinds::{ComponentCapabilities, LogTailResult, ReplacedType};
    use aether_test_fixtures_kinds::{GateProbe, GateQuery};

    use aether_harness_fleet::{FleetHarness, dist_component_available};

    fn has(capabilities: &ComponentCapabilities, id: aether_data::KindId) -> bool {
        capabilities.handlers.iter().any(|handler| handler.id == id)
    }

    fn published<'a>(types: &'a [ReplacedType], namespace: &str) -> &'a ComponentCapabilities {
        &types
            .iter()
            .find(|replaced| replaced.namespace == namespace)
            .unwrap_or_else(|| panic!("the replace reports {namespace}: {types:?}"))
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
        let loaded = harness.load_full_export(engine, "republish_group_v1", "test.republish.gate");

        assert!(has(&loaded.capabilities, GateQuery::ID), "v1 declares GateQuery: {:?}", loaded.capabilities);
        assert!(!has(&loaded.capabilities, GateProbe::ID), "v1 declares no GateProbe: {:?}", loaded.capabilities);

        let types = harness.replace(engine, "republish_group_v2");

        let gate = published(&types, "test.republish.gate");
        assert!(has(gate, GateProbe::ID), "the republished gate declares its added GateProbe row: {gate:?}");
        assert!(has(gate, GateQuery::ID), "the kept GateQuery row survives the republish: {gate:?}");
        assert!(
            matches!(harness.log_tail(engine, &loaded.addr, None, None), LogTailResult::Ok { .. }),
            "the lineage address should still route to the live mailbox after replace",
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
        let courier = harness.load_full_export(engine, "republish_courier_v1", "test.republish.courier");
        let parcel = harness.load_full_export(engine, "republish_courier_v1", "test.republish.parcel");

        let types = harness.replace_with_successor(engine, "republish_courier_v1", 1);

        published(&types, "test.republish.courier");
        published(&types, "test.republish.parcel");
        for address in [&courier.addr, &parcel.addr] {
            assert!(
                matches!(harness.log_tail(engine, address, None, None), LogTailResult::Ok { .. }),
                "{address} should still route to its live mailbox after the republish",
            );
        }
    }
}
