//! `FleetHarness` `describe_component` proof (issue 2421): load a real wasm
//! component into a forked substrate and introspect its ADR-0033
//! receive-side capabilities over the wire, addressed by its ADR-0099
//! lineage name — the externally-addressable surface that makes a
//! boot-manifest-loaded component introspectable without a prior
//! aether-mcp-side `load_component`.

mod tests {
    use aether_data::Kind;
    use aether_kinds::{DescribeComponent, DescribeComponentResult, Tick};
    use aether_test_fixtures_kinds::AssetProbe;

    use aether_harness_fleet::{FleetHarness, dist_component_available};

    /// Load the bundle's `QuietProbe` export, then send
    /// `aether.component.describe` addressed by the lineage name the load
    /// hands back and assert the reply carries the probe's real handler
    /// kinds (`Tick`, `AssetProbe`). This pins the name →
    /// substrate-retained-caps path over the wire: the substrate
    /// resolves the name to its mailbox id and serves the full
    /// `ComponentCapabilities` it retained at load, not the lossy projection.
    #[test]
    fn fleetharness_describe_resolves_caps_by_lineage_name() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let addr = harness.load_full_export(engine, "aether_test_fixtures_bundle", "test.quiet_probe").addr;

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

        // The quiet probe (`test.quiet_probe`) typed-handles Tick and
        // AssetProbe. Asserting both round-trip proves the wire carried the
        // full retained handler set, not an empty stub.
        let handler_ids: Vec<_> = capabilities.handlers.iter().map(|h| h.id).collect();
        assert!(
            handler_ids.contains(&<Tick as Kind>::ID),
            "the described caps should carry the probe's Tick handler, got {handler_ids:?}",
        );
        assert!(
            handler_ids.contains(&<AssetProbe as Kind>::ID),
            "the described caps should carry the probe's AssetProbe handler, got {handler_ids:?}",
        );
    }

    /// The source bytes of the asset the fixture bundle embeds via
    /// `export_asset!("asset_fixture.txt")`, read at compile time so the
    /// length assertion below is a computed tripwire against the exact
    /// bytes the indexer sees in the `aether.asset.asset_fixture.txt`
    /// section.
    const ASSET_FIXTURE: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../aether-test-fixtures-bundle/src/asset_fixture.txt"));

    /// ADR-0163 §3: the asset catalog the load-time indexer built from the
    /// module's `aether.asset.*` custom sections surfaces through
    /// `describe_component`, so tooling reads what a bundle carries without
    /// executing it. Loads the fixture bundle (which `export_asset!`s
    /// `asset_fixture.txt`) and asserts the described caps list that asset
    /// with its byte length.
    #[test]
    fn fleetharness_describe_surfaces_asset_catalog() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let addr = harness.load_full_export(engine, "aether_test_fixtures_bundle", "test.quiet_probe").addr;

        let replies = harness.send(engine, "aether.component", &DescribeComponent { name: addr.clone() });
        let reply = match replies.as_slice() {
            [one] => one,
            other => panic!("describe expected exactly one reply event, got {}", other.len()),
        };
        let capabilities = match DescribeComponentResult::decode_from_bytes(&reply.payload)
            .expect("the reply payload decodes as DescribeComponentResult")
        {
            DescribeComponentResult::Ok { capabilities } => capabilities,
            DescribeComponentResult::Err { error } => {
                panic!("describe by lineage name {addr} should resolve, got Err: {error}")
            }
        };

        let asset = capabilities.assets.iter().find(|a| a.name == "asset_fixture.txt").unwrap_or_else(|| {
            panic!("the described caps should list the export_asset! catalog, got {:?}", capabilities.assets)
        });
        assert_eq!(
            asset.len,
            ASSET_FIXTURE.len() as u64,
            "the indexed asset length should match the embedded source bytes",
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
