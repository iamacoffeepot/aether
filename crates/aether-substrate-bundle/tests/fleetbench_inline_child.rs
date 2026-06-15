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
//!
//! Heavy by construction (fork+exec + cross-process settle) — the test
//! lives in `mod tests::heavy` so nextest's `test(/::heavy::/)` selector
//! serializes it in the `serial-heavy` group.

mod fleetbench;

mod tests {
    mod heavy {
        use aether_data::Kind;
        use aether_test_fixtures::{INLINE_WHO_CHILD, INLINE_WHO_PARENT, InlineEcho, InlineProbe};

        use crate::fleetbench::{FleetBench, dist_manifest_present};

        /// Load `inline_child`, address its inline child by the rendered
        /// lineage name over the wire, and assert the child replied
        /// `InlineEcho { who: CHILD }`; then control-send to the parent's
        /// own address and assert `who: PARENT`. Proves the membrane
        /// demuxed the same `InlineProbe` kind to the child vs the parent
        /// purely on the routed recipient, and that both replies settle
        /// home over the real RPC stack.
        #[test]
        fn fleetbench_inline_child_handles_mail_to_its_lineage_address() {
            if !dist_manifest_present() {
                return;
            }
            let mut bench = FleetBench::start();
            let engine = bench.spawn_headless();
            let parent_addr = bench.load(engine, "inline_child");

            // The child's first-class lineage address: the parent's
            // rendered name plus the inline-child node (ADR-0114).
            let child_addr = format!("{parent_addr}/aether.embedded:widget");

            // Mail to the child's address: the membrane demuxes it to the
            // co-located child, which replies with the CHILD marker.
            let child_replies = bench.send(engine, &child_addr, &InlineProbe);
            let child_reply = match child_replies.as_slice() {
                [one] => one,
                other => panic!(
                    "the inline child should reply exactly once, got {}",
                    other.len(),
                ),
            };
            assert_eq!(
                child_reply.kind,
                InlineEcho::ID,
                "the child reply should be an InlineEcho",
            );
            let echo = InlineEcho::decode_from_bytes(&child_reply.payload)
                .expect("the child reply decodes as InlineEcho");
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
            assert_eq!(
                parent_reply.kind,
                InlineEcho::ID,
                "the parent reply should be an InlineEcho",
            );
            let parent_echo = InlineEcho::decode_from_bytes(&parent_reply.payload)
                .expect("the parent reply decodes as InlineEcho");
            assert_eq!(
                parent_echo.who, INLINE_WHO_PARENT,
                "a normally-addressed actor is unaffected by the inline-child membrane",
            );

            // The child query round-trip is recorded as a CallRecord with
            // the InlineEcho reply, routed to the forked engine.
            let child_record = bench
                .calls()
                .iter()
                .find(|record| {
                    record.request_kind == InlineProbe::ID
                        && record.reply_kinds == vec![InlineEcho::ID]
                })
                .expect("the InlineProbe round-trip is recorded as a CallRecord");
            assert_eq!(
                child_record.engine,
                Some(engine),
                "the InlineProbe is routed to the forked engine",
            );
        }
    }
}
