//! `FleetBench` inline-child proof (issue 1916, ADR-0114 step 5): load a
//! component that spawns a co-located `InlineChild` in `wire`, then send
//! to the child's first-class lineage address **by name over the real
//! `WireFrame::Call` wire** (the same path MCP `send_mail` uses) and
//! assert the *child* — not the parent — handled the query and its reply
//! settled back across the wire. A control send to the parent's own
//! address asserts a normally-addressed actor is unaffected (the membrane
//! no-ops to the parent).
//!
//! This is the headline contract: the inline child is a first-class
//! address reached directly over the wire. `FleetBench` exercises that
//! `Call` path end-to-end; the in-engine mechanism (alias routing,
//! recipient-as-identity, the guest membrane) is covered by the unit
//! tests in `aether-actor` / `aether-substrate`.

mod tests {
    use aether_data::Kind;
    use aether_test_fixtures_kinds::{
        Bump, CONFIGURED_CHILD_INITIAL, CountQuery, CountReport, INLINE_WHO_CHILD, INLINE_WHO_PARENT, InlineEcho,
        InlineProbe,
    };

    use aether_fleet_bench::{FleetBench, dist_component_available};

    /// Load `inline_child`, address its inline child by the rendered
    /// lineage name over the wire, and assert the child replied
    /// `InlineEcho { who: CHILD }`; then control-send to the parent's
    /// own address and assert `who: PARENT`. Proves the membrane
    /// demuxed the same `InlineProbe` kind to the child vs the parent
    /// purely on the routed recipient, and that both replies settle
    /// home over the real RPC stack.
    #[test]
    fn fleetbench_inline_child_handles_mail_to_its_lineage_address() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut bench = FleetBench::start();
        let engine = bench.spawn_headless();
        let parent_addr = bench.load_full_export(engine, "aether_test_fixtures_bundle", "test.inline.parent").addr;

        // The child's first-class lineage address: the parent's
        // rendered name plus the inline-child node (ADR-0114).
        let child_addr = format!("{parent_addr}/aether.embedded:widget");

        // Mail to the child's address: the membrane demuxes it to the
        // co-located child, which replies with the CHILD marker.
        let child_replies = bench.send(engine, &child_addr, &InlineProbe);
        let child_reply = match child_replies.as_slice() {
            [one] => one,
            other => panic!("the inline child should reply exactly once, got {}", other.len()),
        };
        assert_eq!(child_reply.kind, InlineEcho::ID, "the child reply should be an InlineEcho");
        let echo = InlineEcho::decode_from_bytes(&child_reply.payload).expect("the child reply decodes as InlineEcho");
        assert_eq!(
            echo.who, INLINE_WHO_CHILD,
            "the inline child (not the parent) handled the mail to its lineage address",
        );

        // Control: the same kind to the parent's own address is
        // unaffected — the membrane no-ops to the parent, which
        // replies with the PARENT marker.
        let parent_replies = bench.send(engine, &parent_addr, &InlineProbe);
        let parent_reply = match parent_replies.as_slice() {
            [one] => one,
            other => panic!("the parent should reply exactly once, got {}", other.len()),
        };
        assert_eq!(parent_reply.kind, InlineEcho::ID, "the parent reply should be an InlineEcho");
        let parent_echo =
            InlineEcho::decode_from_bytes(&parent_reply.payload).expect("the parent reply decodes as InlineEcho");
        assert_eq!(
            parent_echo.who, INLINE_WHO_PARENT,
            "a normally-addressed actor is unaffected by the inline-child membrane",
        );

        // The child query round-trip is recorded as a CallRecord with
        // the InlineEcho reply, routed to the forked engine.
        let child_record = bench
            .calls()
            .iter()
            .find(|record| record.request_kind == InlineProbe::ID && record.reply_kinds == vec![InlineEcho::ID])
            .expect("the InlineProbe round-trip is recorded as a CallRecord");
        assert_eq!(child_record.engine, Some(engine), "the InlineProbe is routed to the forked engine");
    }

    /// Issue 2690 reload gate: a typed-config inline child's state
    /// survives a `replace_component` swap. Loads `InlineConfiguredParent`
    /// (spawns `InlineConfiguredChild` with a non-default
    /// `InlineConfiguredChildConfig`), bumps the child's counter off its
    /// config-derived starting value, replaces the component in place with
    /// the identical stem/export (the common same-SDK-build swap), and
    /// asserts the moved value — not the config default, not a reset to
    /// `0` — survives. Before the fix, `reconstruct_one_child` re-inited
    /// every child from empty config bytes: a typed (non-`()`) `Config`
    /// decoded `None` there, so the child was dropped outright (not
    /// merely reset) and the post-replace query would have gone
    /// unanswered by the child at all.
    #[test]
    fn fleetbench_inline_configured_child_state_survives_replace() {
        const BUMPS: u32 = 3;
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut bench = FleetBench::start();
        let engine = bench.spawn_headless();
        let parent = bench.load_full_export(engine, "aether_test_fixtures_bundle", "test.inline.configured_parent");
        let child_addr = format!("{}/aether.embedded:widget", parent.addr);

        // Baseline: the child's durable counter starts from the spawn
        // config's `initial`, not `0` — proving the spawn-time config path
        // (not yet the reconstruct path this issue fixes) decoded the real
        // bytes.
        let baseline = count(&mut bench, engine, &child_addr);
        assert_eq!(
            baseline, CONFIGURED_CHILD_INITIAL,
            "the child's counter starts from the spawn config's initial value",
        );

        // Move the state off both the config default (0) and the spawn
        // config's initial value.
        for _ in 0..BUMPS {
            bench.send(engine, &child_addr, &Bump);
        }
        let moved = count(&mut bench, engine, &child_addr);
        let expected_moved = CONFIGURED_CHILD_INITIAL + BUMPS;
        assert_eq!(
            moved, expected_moved,
            "the child's counter moved off both the config default and its initial value",
        );

        // Same-stem in-place replace (ADR-0022): the common in-place
        // rebuild where both sides are the same SDK build (per issue
        // 2690's design notes on the composite bundle's transience).
        bench.replace_export(engine, parent.mailbox_id, "aether_test_fixtures_bundle", "test.inline.configured_parent");

        // The moved state — not the config default, not the spawn
        // config's initial value, not silently dropped — survives the
        // swap: the fix this issue makes.
        let after_replace = count(&mut bench, engine, &child_addr);
        assert_eq!(after_replace, expected_moved, "the typed-config child's moved state survives replace_component");
    }

    /// Send a `CountQuery` to `recipient` and decode the single
    /// `CountReport` reply's `count`. Shared by the reload gate's
    /// baseline / post-bump / post-replace reads.
    fn count(bench: &mut FleetBench, engine: aether_data::EngineId, recipient: &str) -> u32 {
        let replies = bench.send(engine, recipient, &CountQuery);
        let reply = match replies.as_slice() {
            [one] => one,
            other => panic!("CountQuery should get exactly one reply, got {}", other.len()),
        };
        assert_eq!(reply.kind, CountReport::ID, "the reply should be a CountReport");
        CountReport::decode_from_bytes(&reply.payload).expect("the CountReport reply decodes").count
    }
}
