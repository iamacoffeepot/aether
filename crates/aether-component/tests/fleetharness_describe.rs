//! `FleetHarness` `describe_component` proof (issue 2421): load a real wasm
//! component into a forked substrate and introspect its ADR-0033
//! receive-side capabilities over the wire, addressed by its ADR-0099
//! lineage name — the externally-addressable surface that makes a
//! boot-manifest-loaded component introspectable without a prior
//! aether-mcp-side `load_component`.

mod tests {
    use aether_data::Kind;
    use aether_kinds::{DescribeComponent, DescribeComponentResult, Key, Tick};

    use aether_harness_fleet::{FleetHarness, dist_component_available};

    /// Load the `probe` component, then send `aether.component.describe`
    /// addressed by the lineage name `load` hands back and assert the reply
    /// carries the probe's real handler kinds (`Tick`, `Key`). This pins the
    /// name → substrate-retained-caps path over the wire: the substrate
    /// resolves the name to its mailbox id and serves the full
    /// `ComponentCapabilities` it retained at load, not the lossy projection.
    #[test]
    fn fleetharness_describe_resolves_caps_by_lineage_name() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let addr = harness.load(engine, "aether_test_fixtures_bundle");

        let replies = harness.send(engine, "aether.component", &DescribeComponent { name: addr.clone() });
        let reply = match replies.as_slice() {
            [one] => one,
            other => panic!("describe expected exactly one reply event, got {}", other.len()),
        };
        assert_eq!(reply.kind, DescribeComponentResult::ID, "the reply should be a DescribeComponentResult");
        let result = DescribeComponentResult::decode_from_bytes(&reply.payload)
            .expect("the reply payload decodes as DescribeComponentResult");
        let capabilities = match result {
            DescribeComponentResult::Ok { capabilities } => capabilities,
            DescribeComponentResult::Err { error } => {
                panic!("describe by lineage name {addr} should resolve, got Err: {error}")
            }
        };

        // The probe entry actor (`test.probe`) typed-handles Tick,
        // Key, and SetRender. Asserting two substrate kinds round-trip proves
        // the wire carried the full retained handler set, not an empty stub.
        let handler_ids: Vec<_> = capabilities.handlers.iter().map(|h| h.id).collect();
        assert!(
            handler_ids.contains(&<Tick as Kind>::ID),
            "the described caps should carry the probe's Tick handler, got {handler_ids:?}",
        );
        assert!(
            handler_ids.contains(&<Key as Kind>::ID),
            "the described caps should carry the probe's Key handler, got {handler_ids:?}",
        );
    }

    /// Describing an unregistered lineage name is a definitive
    /// `DescribeComponentResult::Err`, not a hang or a panic — the
    /// fail-fast negative path.
    #[test]
    fn fleetharness_describe_unknown_name_errs() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();

        let replies = harness.send(
            engine,
            "aether.component",
            &DescribeComponent { name: "aether.component/aether.embedded:nonexistent".to_owned() },
        );
        let reply = match replies.as_slice() {
            [one] => one,
            other => panic!("describe expected exactly one reply event, got {}", other.len()),
        };
        let result = DescribeComponentResult::decode_from_bytes(&reply.payload)
            .expect("the reply payload decodes as DescribeComponentResult");
        assert!(matches!(result, DescribeComponentResult::Err { .. }), "an unregistered name should describe as Err");
    }
}
