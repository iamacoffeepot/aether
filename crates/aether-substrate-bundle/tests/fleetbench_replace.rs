//! `FleetBench` `replace_component` proof (issue 1459, Tier-A): load the
//! `probe` fixture into a forked substrate, then atomically swap it for
//! the standalone stateful fixture's entry actor at the
//! same trampoline mailbox id (ADR-0022). Assert the returned capability
//! set reflects the cross-module swap, the lineage address stays put,
//! and mail reaches the replacement guest.

mod fleetbench;

mod tests {
    use aether_actor::Addressable;
    use aether_capabilities::WasmTrampoline;
    use aether_data::Kind;
    use aether_kinds::{ComponentCapabilities, LogTailResult, Ping};
    use aether_test_fixtures_kinds::{Bump, CountQuery, CountReport, SetRender};

    use crate::fleetbench::{FleetBench, dist_component_available};

    /// Load the bundled `probe` (handler `SetRender`), then replace it
    /// with the standalone `aether_test_fixtures_stateful_typed` module's
    /// entry actor (handlers `Bump` + `CountQuery`) at the
    /// captured trampoline `mailbox_id`. This exercises the
    /// cross-module `ReplaceComponent` path over the real wire. The returned
    /// capabilities must flip to the stateful handler set, while a `Bump`
    /// followed by `CountQuery` at the probe's unchanged load-time lineage
    /// address must report `count = 1`. The neighboring test covers a named
    /// non-entry export replacement separately.
    #[test]
    fn fleetbench_replaces_probe_with_stateful_actor_at_a_stable_address() {
        if !dist_component_available("aether_test_fixtures_bundle")
            || !dist_component_available("aether_test_fixtures_stateful_typed")
        {
            return;
        }
        let mut bench = FleetBench::start();
        let engine = bench.spawn_headless();
        let loaded = bench.load_full(engine, "aether_test_fixtures_bundle");

        let has = |caps: &ComponentCapabilities, id| caps.handlers.iter().any(|h| h.id == id);

        // Pre-replace sanity: the bundled probe declares SetRender, not
        // the standalone stateful actor's driver/query kinds, and
        // registers at its ADR-0099 lineage address.
        assert!(
            has(&loaded.capabilities, SetRender::ID),
            "probe should declare a SetRender handler: {:?}",
            loaded.capabilities.handlers,
        );
        assert!(
            !has(&loaded.capabilities, Bump::ID),
            "probe should not declare a Bump handler: {:?}",
            loaded.capabilities.handlers,
        );
        assert!(
            !has(&loaded.capabilities, CountQuery::ID),
            "probe should not declare a CountQuery handler: {:?}",
            loaded.capabilities.handlers,
        );
        let expected = format!("aether.component/{}:test.probe", WasmTrampoline::NAMESPACE);
        assert_eq!(loaded.addr, expected, "probe should load at its ADR-0099 lineage address");

        let caps = bench.replace(engine, loaded.mailbox_id, "aether_test_fixtures_stateful_typed");

        // Post-replace: the standalone stateful actor's handlers are
        // active and the bundled probe's distinguishing handler is gone.
        assert!(has(&caps, Bump::ID), "post-replace should declare a Bump handler: {:?}", caps.handlers);
        assert!(has(&caps, CountQuery::ID), "post-replace should declare a CountQuery handler: {:?}", caps.handlers);
        assert!(
            !has(&caps, SetRender::ID),
            "post-replace should not declare the probe's SetRender handler: {:?}",
            caps.handlers,
        );

        // Drive replacement-only guest behavior through the probe's
        // load-time lineage address. A decoded count of one proves both
        // mails reached the stateful module at the same trampoline path.
        let bump_replies = bench.send(engine, &loaded.addr, &Bump);
        assert!(bump_replies.is_empty(), "Bump should not reply, got {bump_replies:?}");
        let replies = bench.send(engine, &loaded.addr, &CountQuery);
        let reply = match replies.as_slice() {
            [one] => one,
            other => panic!("CountQuery should get exactly one reply, got {}", other.len()),
        };
        assert_eq!(reply.kind, CountReport::ID, "the replacement should reply with CountReport");
        let report = CountReport::decode_from_bytes(&reply.payload).expect("the CountReport reply should decode");
        assert_eq!(report.count, 1, "one Bump at the stable lineage address should advance the replacement state");

        // The framework-owned LogTail handler remains another direct
        // liveness check through that unchanged address.
        assert!(
            matches!(bench.log_tail(engine, &loaded.addr, None, None), LogTailResult::Ok { .. },),
            "the lineage address should still route to the live mailbox after replace",
        );
    }

    /// ADR-0096 wire regression for `ReplaceComponent.export`: load
    /// the `multi_actor` module's **entry** actor (`RootManager`, a
    /// strict receiver — no `#[fallback]`), then replace it with the
    /// non-entry export `test.ui.panel` (`Panel`, which carries a
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
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut bench = FleetBench::start();
        let engine = bench.spawn_headless();

        // Load the `RootManager` actor (a strict receiver) from the
        // bundle by its `test.ui.root` export. It is a non-entry actor in the
        // bundle (the entry is `Probe`), so it is selected explicitly.
        let loaded = bench.load_full_export(engine, "aether_test_fixtures_bundle", "test.ui.root");

        // Pre-replace: the entry is a strict receiver — it declares a
        // Ping handler and no fallback.
        assert!(
            loaded.capabilities.handlers.iter().any(|h| h.id == Ping::ID),
            "the entry RootManager should declare a Ping handler: {:?}",
            loaded.capabilities.handlers,
        );
        assert!(
            loaded.capabilities.fallback.is_none(),
            "the entry RootManager is a strict receiver — no fallback: {:?}",
            loaded.capabilities.fallback,
        );

        // Replace into the non-entry export `test.ui.panel`, at the same
        // mailbox id, carrying the export over the wire.
        let caps = bench.replace_export(engine, loaded.mailbox_id, "aether_test_fixtures_bundle", "test.ui.panel");

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
            matches!(bench.log_tail(engine, &loaded.addr, None, None), LogTailResult::Ok { .. },),
            "the lineage address should still route to the live mailbox after an \
                 export-targeted replace",
        );
    }
}
