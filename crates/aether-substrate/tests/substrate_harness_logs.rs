//! `SubstrateHarness` actor-log reader proof (issue 1856; issue 7107): load
//! the bundle's `QuietProbe` export, send it a `LogMarker` to fire its
//! `tracing::info!`, tail its per-actor `ActorLogRing` (ADR-0081) for the
//! `typed_send_alive` info entry, then walk the `since` cursor to confirm it
//! does not re-yield the seen entry.

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor` entries are
// present in this test binary (same rationale as cap_registry.rs).
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

mod tests {
    use std::fs;

    use aether_harness_substrate::test_helpers::require_wasm;
    use aether_harness_substrate::{HarnessOp, SubstrateHarness};
    use aether_kinds::{LoadComponent, LogTailResult};
    use aether_test_fixtures_bundle::QuietProbe;
    use aether_test_fixtures_kinds::LogMarker;

    /// `info` in the `0 = trace .. 4 = error` level mapping shared across
    /// `aether.log.*`.
    const LEVEL_INFO: u8 = 2;

    /// Load `probe`, send it a `LogMarker`, read its reference once with
    /// `SubstrateHarness::log_tail` for the `typed_send_alive` info entry,
    /// then re-query past the returned cursor and assert it is not
    /// re-yielded — the in-process counterpart to
    /// `fleetharness_actor_logs_surface_the_probe_marker_entry`.
    #[test]
    fn substrate_harness_actor_logs_surface_the_probe_marker_entry() {
        let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
            return;
        };
        let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

        let wasm = fs::read(&wasm_path).expect("read probe wasm");
        let probe = harness
            .load::<QuietProbe>(LoadComponent { wasm, name: None, config: Vec::new(), export: None })
            .expect("load probe");

        harness.execute(vec![("marker", HarnessOp::send_and_settle(&probe, &LogMarker))]).expect("send LogMarker");

        // The guest's log host fn pushes into the actor's log ring on the
        // dispatcher thread, inside the handler, and `send_and_settle`
        // returns only once the marker's subtree settled — so one read sees
        // the entry.
        let reply = harness.log_tail(&probe, None, None);
        let LogTailResult::Ok { ref entries, next_since, .. } = reply else {
            panic!("LogTail failed: {reply:?}");
        };
        let entry = entries
            .iter()
            .find(|e| e.message == "typed_send_alive" && e.level == LEVEL_INFO)
            .unwrap_or_else(|| panic!("probe's `typed_send_alive` info entry is not in the ring: {reply:?}"))
            .clone();

        assert!(entry.sequence >= 1, "a buffered entry should carry a 1-based ring sequence, got {}", entry.sequence);

        // Walk the cursor: a re-query past `next_since` must not re-yield
        // the entry we already consumed.
        match harness.log_tail(&probe, Some(next_since), None) {
            LogTailResult::Ok { entries, .. } => assert!(
                entries.iter().all(|e| e.sequence != entry.sequence),
                "the `since` cursor should not re-yield the already-seen entry \
                     (seq {}): {entries:?}",
                entry.sequence,
            ),
            LogTailResult::Err { error } => {
                panic!("cursor re-query LogTail failed: {error}")
            }
        }
    }
}
