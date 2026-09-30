//! `FleetHarness` reply-routing regression for the two **held**
//! component-lifecycle ops (issue 1466). Over the real hub → RPC →
//! forked-substrate wire, the component host holds the reply to a `Publish`
//! of a successor across a republish's prepare, publish and commit turns
//! (ADR-0241 §3, §7), and hands `DropComponent`'s to the trampoline; either
//! reply must stream back before the originating call settles. Before the
//! issue-1466 fix a deferred reply did not hold the call's trace root open,
//! so the call emitted `ReplyEnd(Ok)` with zero reply events and the
//! `PublishResult` / `DropResult` routed to a call that had already closed
//! (discarded).

mod tests {
    use aether_data::{Blob, Kind};
    use aether_kinds::{DropComponent, DropResult, LoadComponent, LoadResult, Publish, PublishResult};
    use aether_rpc::ReplyEnvelope;
    use aether_substrate::testing::successor_wasm;

    use aether_harness_fleet::{FleetHarness, dist_component_available, read_component_wasm};

    /// Load the `probe` component, republish its module with identical code
    /// under a new hash, and drop it. Each held operation draws its
    /// `*Result::Ok` as a streamed reply event ahead of `ReplyEnd`; before
    /// the issue-1466 fix the call settled before the answer, so the reply
    /// set came back empty. `QuietProbe` declares only dependencies headless
    /// serves.
    #[test]
    fn replace_and_drop_route_their_held_reply() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let wasm = read_component_wasm("aether_test_fixtures_bundle");

        // Load the probe and read its actor path off the LoadResult; the
        // drop addresses the trampoline by that path.
        let load_replies = harness.send::<LoadComponent>(
            engine,
            "aether.component",
            &LoadComponent {
                wasm: wasm.clone(),
                name: None,
                config: Vec::new(),
                export: Some("test.quiet_probe".to_owned()),
            },
        );
        let path = match decode_reply::<LoadResult>(&load_replies) {
            LoadResult::Ok { path, .. } => path,
            LoadResult::Err { error } => panic!("probe load failed: {error}"),
        };

        // The reply set is empty before the issue-1466 fix (`ReplyEnd` with
        // zero events).
        let publish_replies = harness.send::<Publish>(
            engine,
            "aether.component",
            &Publish { code: Blob::from(successor_wasm(&wasm, 1)), configs: Vec::new() },
        );
        assert!(
            !publish_replies.is_empty(),
            "Publish drew zero reply events — the call settled before the held reply answered (issue 1466)",
        );
        match decode_reply::<PublishResult>(&publish_replies) {
            PublishResult::Ok { .. } => {}
            PublishResult::Err { error } => panic!("publish of the successor failed: {error}"),
        }

        // The loaded guest publishes only its own rows, which do not include
        // the native DropComponent handler. The host-owned proof retained at
        // spawn, and kept across the republish, must still make the drop
        // succeed.
        let drop_replies = harness.send::<DropComponent>(engine, "aether.component", &DropComponent { target: path });
        assert!(
            !drop_replies.is_empty(),
            "DropComponent drew zero reply events — the handed-off reply settled before the trampoline replied (issue 1466)",
        );
        match decode_reply::<DropResult>(&drop_replies) {
            DropResult::Ok => {}
            DropResult::Err { error } => panic!("drop failed: {error}"),
        }
    }

    /// Decode the single reply envelope of kind `R` from a call's
    /// reply set, panicking if it is absent or undecodable.
    fn decode_reply<R: Kind>(replies: &[ReplyEnvelope]) -> R {
        let envelope = replies
            .iter()
            .find(|e| e.kind == R::ID)
            .unwrap_or_else(|| panic!("no reply of kind {} in the reply set", R::NAME));
        R::decode_from_bytes(&envelope.payload).unwrap_or_else(|| panic!("undecodable {} reply", R::NAME))
    }
}
