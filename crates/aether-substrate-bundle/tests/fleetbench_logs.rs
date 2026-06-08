//! `FleetBench` `actor_logs` proof (issue 1459, Tier-A): load the
//! `probe` fixture into a forked substrate and tail its per-actor
//! `ActorLogRing` (ADR-0081) for the one-shot `typed_send_alive` entry
//! the probe emits on its first tick, then walk the `since` cursor to
//! confirm it does not re-yield the seen entry.
//!
//! Heavy by construction (fork+exec + cross-process settle) — the test
//! lives in `mod tests::heavy` so nextest's `test(/::heavy::/)` selector
//! serializes it in the `serial-heavy` group.

mod fleetbench;

mod tests {
    mod heavy {
        use std::thread;
        use std::time::Duration;

        use aether_kinds::LogTailResult;

        use crate::fleetbench::FleetBench;

        /// Up to ~3s of bounded polling closes the wire→first-tick race:
        /// the headless chassis auto-ticks at 60Hz, so the probe's first
        /// tick (which emits the entry) fires within a few frames of
        /// `wire`, and the lone entry is never evicted from a 100-slot
        /// ring.
        const POLL_ATTEMPTS: usize = 30;
        const POLL_INTERVAL: Duration = Duration::from_millis(100);

        /// `info` in the `0 = trace .. 4 = error` level mapping shared
        /// across `aether.log.*`.
        const LEVEL_INFO: u8 = 2;

        /// Load `probe`, poll its lineage address with `LogTail` until the
        /// `typed_send_alive` info entry appears, then re-query past the
        /// returned cursor and assert it is not re-yielded — the
        /// `actor_logs` row: a per-actor ring read plus a `since`-cursor
        /// walk.
        #[test]
        fn fleetbench_actor_logs_surface_the_probe_first_tick_entry() {
            let mut bench = FleetBench::start();
            let engine = bench.spawn_headless();
            let addr = bench.load(engine, "probe");

            let mut last_reply = None;
            let mut found = None;
            for _ in 0..POLL_ATTEMPTS {
                let reply = bench.log_tail(engine, &addr, None);
                if let LogTailResult::Ok {
                    entries,
                    next_since,
                    ..
                } = &reply
                    && let Some(entry) = entries
                        .iter()
                        .find(|e| e.message == "typed_send_alive" && e.level == LEVEL_INFO)
                {
                    found = Some((entry.clone(), *next_since));
                    break;
                }
                last_reply = Some(reply);
                thread::sleep(POLL_INTERVAL);
            }

            let (entry, next_since) = found.unwrap_or_else(|| {
                panic!(
                    "probe's `typed_send_alive` info entry never appeared after {POLL_ATTEMPTS} \
                     polls; last reply: {last_reply:?}",
                )
            });

            // The ring's per-actor sequence starts at 1 (ADR-0081).
            assert!(
                entry.sequence >= 1,
                "a buffered entry should carry a 1-based ring sequence, got {}",
                entry.sequence,
            );

            // Walk the cursor: a re-query past `next_since` must not
            // re-yield the entry we already consumed.
            match bench.log_tail(engine, &addr, Some(next_since)) {
                LogTailResult::Ok { entries, .. } => assert!(
                    entries.iter().all(|e| e.sequence != entry.sequence),
                    "the `since` cursor should not re-yield the already-seen entry (seq {}): {entries:?}",
                    entry.sequence,
                ),
                LogTailResult::Err { error } => panic!("cursor re-query LogTail failed: {error}"),
            }
        }
    }
}
