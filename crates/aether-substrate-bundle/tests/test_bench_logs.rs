//! `TestBench` actor-log reader proof (issue 1856): load the `probe` fixture,
//! advance one tick to fire its first-tick `tracing::info!`, tail its per-actor
//! `ActorLogRing` (ADR-0081) for the `typed_send_alive` info entry, then walk
//! the `since` cursor to confirm it does not re-yield the seen entry.
//!
//! Heavy by construction (full `TestBench` chassis + wasm compile) — lives in
//! `mod tests::heavy` so nextest's `test(/::heavy::/)` selector serializes it
//! in the `serial-heavy` group.

// Skip diagnostics emit `eprintln!` so `cargo test` runners surface a visible
// "skipping: ..." line alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor` entries are
// present in this test binary (same rationale as test_bench_scenario.rs).
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

mod tests {
    mod heavy {
        use std::fs;
        use std::thread;
        use std::time::Duration;

        use aether_actor::Addressable;
        use aether_kinds::{LoadComponent, LoadResult, LogTailResult};
        use aether_substrate_bundle::test_bench::{
            BenchOp, TestBench, test_helpers::require_runtime,
        };

        const PROBE_NAME: &str = "probe";

        fn probe_address() -> String {
            format!(
                "aether.component/{}:{}",
                aether_capabilities::WasmTrampoline::NAMESPACE,
                PROBE_NAME,
            )
        }

        /// `info` in the `0 = trace .. 4 = error` level mapping shared across
        /// `aether.log.*`.
        const LEVEL_INFO: u8 = 2;

        /// Polling budget: after `advance(1)` the entry should already be in the
        /// ring (the tick settled before `advance` returned), but a few retries
        /// absorb any edge-case timing without lengthening the happy path.
        const POLL_ATTEMPTS: usize = 10;
        const POLL_INTERVAL: Duration = Duration::from_millis(50);

        /// Load `probe`, advance one tick, poll its lineage address with
        /// `TestBench::log_tail` until the `typed_send_alive` info entry appears,
        /// then re-query past the returned cursor and assert it is not
        /// re-yielded — the in-process counterpart to
        /// `fleetbench_actor_logs_surface_the_probe_first_tick_entry`.
        #[test]
        fn test_bench_actor_logs_surface_the_probe_first_tick_entry() {
            let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
                return;
            };
            let mut bench = match TestBench::start_with_size(64, 48) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("skipping: TestBench boot failed (likely no wgpu adapter): {e}");
                    return;
                }
            };

            let wasm = fs::read(&wasm_path).expect("read probe wasm");
            let loaded = bench
                .execute(vec![(
                    "load",
                    BenchOp::send_and_await(
                        "aether.component",
                        &LoadComponent {
                            wasm,
                            name: Some(PROBE_NAME.to_owned()),
                            config: Vec::new(),
                            export: None,
                        },
                    ),
                )])
                .expect("load probe");
            match loaded
                .reply::<LoadResult>("load")
                .expect("decode LoadResult")
            {
                LoadResult::Ok { .. } => {}
                LoadResult::Err { error } => panic!("load_component: {error}"),
            }

            bench
                .execute(vec![("tick", BenchOp::advance(1))])
                .expect("advance one tick");

            let addr = probe_address();
            let mut last_reply = None;
            let mut found = None;
            for _ in 0..POLL_ATTEMPTS {
                let reply = bench.log_tail(&addr, None);
                if let LogTailResult::Ok {
                    ref entries,
                    next_since,
                    ..
                } = reply
                    && let Some(entry) = entries
                        .iter()
                        .find(|e| e.message == "typed_send_alive" && e.level == LEVEL_INFO)
                {
                    found = Some((entry.clone(), next_since));
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

            assert!(
                entry.sequence >= 1,
                "a buffered entry should carry a 1-based ring sequence, got {}",
                entry.sequence,
            );

            // Walk the cursor: a re-query past `next_since` must not re-yield
            // the entry we already consumed.
            match bench.log_tail(&addr, Some(next_since)) {
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
}
