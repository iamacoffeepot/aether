//! `FleetBench` reply-routing regression for the two **forwarded**
//! component-lifecycle ops (issue 1466). Over the real hub → RPC →
//! forked-substrate wire, `ReplaceComponent` and `DropComponent` are
//! forwarded by the component cap to the trampoline, whose deferred
//! `ctx.reply` must stream back before the originating call settles.
//! Before the fix the forward did not hold the call's trace root open,
//! so the call emitted `ReplyEnd(Ok)` with zero reply events and the
//! `ReplaceResult` / `DropResult` routed to a call that had already
//! closed (discarded). `load_component` is unaffected — the cap answers
//! it inline, streaming the reply home before settlement.
//!
//! Heavy by construction (fork+exec + cross-process settle) — the test
//! lives in `mod tests::heavy` so nextest's `test(/::heavy::/)` selector
//! serializes it in the `serial-heavy` group.

mod fleetbench;

mod tests {
    mod heavy {
        use aether_capabilities::rpc::MailEnvelope;
        use aether_data::Kind;
        use aether_kinds::{
            DropComponent, DropResult, LoadComponent, LoadResult, ReplaceComponent, ReplaceResult,
        };

        use crate::fleetbench::{FleetBench, dist_manifest_present, read_component_wasm};

        /// Load the `probe` component, then drive a `ReplaceComponent`
        /// and a `DropComponent` to its cap over the real wire and assert
        /// each draws its `*Result::Ok` as a streamed reply event ahead
        /// of `ReplyEnd`. Both ops route through the component cap's
        /// `forward_to_trampoline`; before the issue-1466 fix the forward
        /// let the call settle before the trampoline replied, so the
        /// reply set came back empty.
        #[test]
        fn forwarded_replace_and_drop_route_their_reply() {
            if !dist_manifest_present() {
                return;
            }
            let mut bench = FleetBench::start();
            let engine = bench.spawn_headless();
            let wasm = read_component_wasm("probe");

            // Load the probe and read its mailbox id off the LoadResult —
            // the harness `load` helper returns only the registered name,
            // and the forwarded ops address the trampoline by id.
            let load_replies = bench.send::<LoadComponent>(
                engine,
                "aether.component",
                &LoadComponent {
                    wasm: wasm.clone(),
                    name: None,
                    config: Vec::new(),
                    export: None,
                },
            );
            let mailbox_id = match decode_reply::<LoadResult>(&load_replies) {
                LoadResult::Ok { mailbox_id, .. } => mailbox_id,
                LoadResult::Err { error } => panic!("probe load failed: {error}"),
            };

            // Replace probe-with-probe. The reply set is empty before the
            // fix (`ReplyEnd` with zero events); populated after.
            let replace_replies = bench.send::<ReplaceComponent>(
                engine,
                "aether.component",
                &ReplaceComponent {
                    mailbox_id,
                    wasm,
                    drain_timeout_ms: None,
                    config: Vec::new(),
                    export: None,
                },
            );
            assert!(
                !replace_replies.is_empty(),
                "ReplaceComponent drew zero reply events — the forwarded reply settled before the trampoline replied (issue 1466)",
            );
            match decode_reply::<ReplaceResult>(&replace_replies) {
                ReplaceResult::Ok { .. } => {}
                ReplaceResult::Err { error } => panic!("replace failed: {error}"),
            }

            // Drop shares the forwarded path — assert it routes its reply
            // too. The mailbox id is stable across the replace (ADR-0022).
            let drop_replies = bench.send::<DropComponent>(
                engine,
                "aether.component",
                &DropComponent { mailbox_id },
            );
            assert!(
                !drop_replies.is_empty(),
                "DropComponent drew zero reply events — the forwarded reply settled before the trampoline replied (issue 1466)",
            );
            match decode_reply::<DropResult>(&drop_replies) {
                DropResult::Ok => {}
                DropResult::Err { error } => panic!("drop failed: {error}"),
            }
        }

        /// Decode the single reply envelope of kind `R` from a call's
        /// reply set, panicking if it is absent or undecodable.
        fn decode_reply<R: Kind>(replies: &[MailEnvelope]) -> R {
            let envelope = replies
                .iter()
                .find(|e| e.kind == R::ID)
                .unwrap_or_else(|| panic!("no reply of kind {} in the reply set", R::NAME));
            R::decode_from_bytes(&envelope.payload)
                .unwrap_or_else(|| panic!("undecodable {} reply", R::NAME))
        }
    }
}
