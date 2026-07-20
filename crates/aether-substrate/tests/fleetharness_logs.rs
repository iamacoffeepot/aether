//! `FleetHarness` `actor_logs` proof (issue 1459, Tier-A): load the
//! `probe` fixture into a forked substrate and tail its per-actor
//! `ActorLogRing` (ADR-0081) for the one-shot `typed_send_alive` entry
//! the probe emits on its first tick, assert the substrate-side substring
//! filter across the real RPC hop, then walk the `since` cursor to confirm
//! it does not re-yield the seen entry.

mod tests {
    use aether_kinds::LogTailResult;

    use aether_harness_fleet::{FleetHarness, dist_component_available, poll_until};

    /// `info` in the `0 = trace .. 4 = error` level mapping shared
    /// across `aether.log.*`.
    const LEVEL_INFO: u8 = 2;
    const MESSAGE_SUBSTRING: &str = "typed_send_alive";

    /// Load `probe`, poll its lineage address with `LogTail` until the
    /// `typed_send_alive` info entry appears, then re-query past the
    /// returned cursor and assert it is not re-yielded — the
    /// `actor_logs` row: a per-actor ring read plus a `since`-cursor
    /// walk.
    #[test]
    fn fleetharness_actor_logs_surface_the_probe_first_tick_entry() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let addr = harness.load(engine, "aether_test_fixtures_bundle");

        let mut last_reply = None;
        let mut found = None;
        poll_until(|| {
            let reply = harness.log_tail(engine, &addr, None, Some(MESSAGE_SUBSTRING.to_owned()));
            if let LogTailResult::Ok { entries, .. } = &reply {
                assert!(
                    entries.iter().all(|entry| entry.message.contains(MESSAGE_SUBSTRING)),
                    "the substrate-side contains filter must remove non-matching entries before the RPC reply: {entries:?}",
                );
            }
            if let LogTailResult::Ok { entries, next_since, .. } = &reply
                && let Some(entry) = entries.iter().find(|e| e.message == "typed_send_alive" && e.level == LEVEL_INFO)
            {
                found = Some((entry.clone(), *next_since));
                true
            } else {
                last_reply = Some(reply);
                false
            }
        });

        let (entry, next_since) = found.unwrap_or_else(|| {
            panic!(
                "probe's `typed_send_alive` info entry never appeared within the poll \
                 budget; last reply: {last_reply:?}",
            )
        });

        // The ring's per-actor sequence starts at 1 (ADR-0081).
        assert!(entry.sequence >= 1, "a buffered entry should carry a 1-based ring sequence, got {}", entry.sequence);

        // Walk the cursor: a re-query past `next_since` must not
        // re-yield the entry we already consumed.
        match harness.log_tail(engine, &addr, Some(next_since), Some(MESSAGE_SUBSTRING.to_owned())) {
            LogTailResult::Ok { entries, .. } => assert!(
                entries.iter().all(|e| e.sequence != entry.sequence),
                "the `since` cursor should not re-yield the already-seen entry (seq {}): {entries:?}",
                entry.sequence,
            ),
            LogTailResult::Err { error } => panic!("cursor re-query LogTail failed: {error}"),
        }
    }
}
